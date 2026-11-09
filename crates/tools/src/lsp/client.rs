use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};
use serde_json::{json, Value};
use goldenagent_core::{LspDiagnostic, LspLocation, LspPosition};

use super::jsonrpc::{decode_messages, encode_message};

pub fn path_to_uri(path: &Path) -> String {
    let path_str = path.to_string_lossy().replace('\\', "/");
    let mut s = path_str.as_str();
    if let Some(stripped) = s.strip_prefix("//?/") {
        s = stripped;
    } else if let Some(stripped) = s.strip_prefix("/?/") {
        s = stripped;
    }
    let s = s.trim_start_matches('/');
    format!("file:///{s}")
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let mut rest = uri.strip_prefix("file://")?;
    rest = rest.trim_start_matches('/');
    if let Some(stripped) = rest.strip_prefix("?/") {
        rest = stripped;
    }
    let clean = if cfg!(windows) && rest.starts_with('/') && rest.chars().nth(2) == Some(':') {
        &rest[1..]
    } else {
        rest
    };
    Some(PathBuf::from(clean.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

type ResponseWaiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

enum Transport {
    Process {
        stdin: Arc<Mutex<tokio::process::ChildStdin>>,
        _child: Arc<Mutex<tokio::process::Child>>,
    },
    Mock {
        handler: Arc<dyn Fn(&Value) -> Option<Value> + Send + Sync>,
    },
}

pub struct LspClient {
    transport: Transport,
    next_id: AtomicU64,
    waiters: ResponseWaiters,
    diagnostics_store: Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>>,
    pub root_path: PathBuf,
}

impl LspClient {
    /// 启动一个真实的语言服务器子进程（如 rust-analyzer / tsserver 等）。
    pub async fn spawn(command: &str, args: &[&str], root: impl Into<PathBuf>) -> Result<Arc<Self>, String> {
        let root_path = root.into();
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .current_dir(&root_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().map_err(|e| {
            format!("启动语言服务器 '{command}' 失败: {e}。请确认已安装并在系统 PATH 中。")
        })?;

        let stdin = child.stdin.take().ok_or_else(|| "无法获取子进程 stdin".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "无法获取子进程 stdout".to_string())?;

        let waiters: ResponseWaiters = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics_store = Arc::new(Mutex::new(HashMap::new()));

        // 后台任务：持续从 stdout 读取分帧并分发响应/通知
        let waiters_clone = Arc::clone(&waiters);
        let diag_clone = Arc::clone(&diagnostics_store);
        tokio::spawn(async move {
            let mut reader = stdout;
            let mut read_buf = [0u8; 4096];
            let mut stream_buf = Vec::new();

            loop {
                match reader.read(&mut read_buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        stream_buf.extend_from_slice(&read_buf[..n]);
                        if let Ok(msgs) = decode_messages(&mut stream_buf) {
                            for msg in msgs {
                                Self::dispatch_incoming(msg, &waiters_clone, &diag_clone).await;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let client = Arc::new(Self {
            transport: Transport::Process {
                stdin: Arc::new(Mutex::new(stdin)),
                _child: Arc::new(Mutex::new(child)),
            },
            next_id: AtomicU64::new(1),
            waiters,
            diagnostics_store,
            root_path: root_path.clone(),
        });

        // 执行握手流程
        client.initialize(&root_path).await?;
        Ok(client)
    }

    /// 创建用于单元/集成测试的 Mock LSP 客户端。
    pub fn mock<F>(root: impl Into<PathBuf>, handler: F) -> Arc<Self>
    where
        F: Fn(&Value) -> Option<Value> + Send + Sync + 'static,
    {
        Arc::new(Self {
            transport: Transport::Mock {
                handler: Arc::new(handler),
            },
            next_id: AtomicU64::new(1),
            waiters: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_store: Arc::new(Mutex::new(HashMap::new())),
            root_path: root.into(),
        })
    }

    async fn dispatch_incoming(
        msg: Value,
        waiters: &ResponseWaiters,
        diagnostics_store: &Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>>,
    ) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let mut lock = waiters.lock().await;
            if let Some(tx) = lock.remove(&id) {
                if let Some(err) = msg.get("error") {
                    let err_msg = err.get("message").and_then(Value::as_str).unwrap_or("LSP 远程返回错误");
                    let _ = tx.send(Err(err_msg.to_string()));
                } else {
                    let result = msg.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(Ok(result));
                }
            }
        } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
            // 处理通知
            if method == "textDocument/publishDiagnostics" {
                if let Some(params) = msg.get("params") {
                    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                        let mut diags = Vec::new();
                        if let Some(items) = params.get("diagnostics").and_then(Value::as_array) {
                            for item in items {
                                let range = item.get("range").and_then(Value::as_object);
                                let start = range.and_then(|r| r.get("start")).and_then(Value::as_object);
                                let l0 = start.and_then(|s| s.get("line")).and_then(Value::as_u64).unwrap_or(0) as u32;
                                let c0 = start.and_then(|s| s.get("character")).and_then(Value::as_u64).unwrap_or(0) as u32;
                                let pos = LspPosition::from_lsp_zero_based(l0, c0);

                                let sev_num = item.get("severity").and_then(Value::as_u64).unwrap_or(1);
                                let severity = match sev_num {
                                    1 => "Error",
                                    2 => "Warning",
                                    3 => "Information",
                                    _ => "Hint",
                                };
                                let message = item.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                                let code = item.get("code").map(|c| c.to_string());
                                let file_path = uri_to_path(uri).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| uri.to_string());

                                diags.push(LspDiagnostic::new(file_path, pos.line, pos.character, severity, message, code));
                            }
                        }
                        let mut store = diagnostics_store.lock().await;
                        store.insert(uri.to_string(), diags);
                    }
                }
            }
        }
    }

    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        match &self.transport {
            Transport::Process { stdin, .. } => {
                let (tx, rx) = oneshot::channel();
                {
                    let mut lock = self.waiters.lock().await;
                    lock.insert(id, tx);
                }

                let payload = encode_message(&req);
                {
                    let mut writer = stdin.lock().await;
                    writer.write_all(&payload).await.map_err(|e| format!("发送 LSP 请求失败: {e}"))?;
                    writer.flush().await.map_err(|e| format!("刷新 LSP 请求失败: {e}"))?;
                }

                match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
                    Ok(Ok(res)) => res,
                    Ok(Err(_)) => Err("LSP 通信通道已关闭".to_string()),
                    Err(_) => {
                        let mut lock = self.waiters.lock().await;
                        lock.remove(&id);
                        Err("LSP 请求超时 (10s)".to_string())
                    }
                }
            }
            Transport::Mock { handler } => {
                if let Some(resp) = handler(&req) {
                    if let Some(err) = resp.get("error") {
                        let err_msg = err.get("message").and_then(Value::as_str).unwrap_or("Mock LSP 错误");
                        Err(err_msg.to_string())
                    } else {
                        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
                    }
                } else {
                    Ok(Value::Null)
                }
            }
        }
    }

    pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        match &self.transport {
            Transport::Process { stdin, .. } => {
                let payload = encode_message(&notif);
                let mut writer = stdin.lock().await;
                writer.write_all(&payload).await.map_err(|e| format!("发送 LSP 通知失败: {e}"))?;
                writer.flush().await.map_err(|e| format!("刷新 LSP 通知失败: {e}"))?;
                Ok(())
            }
            Transport::Mock { handler } => {
                let _ = handler(&notif);
                Ok(())
            }
        }
    }

    /// 初始化握手
    pub async fn initialize(&self, root_path: &Path) -> Result<Value, String> {
        let uri = path_to_uri(root_path);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": {
                "textDocument": {
                    "definition": { "dynamicRegistration": false },
                    "references": { "dynamicRegistration": false },
                    "rename": { "dynamicRegistration": false },
                    "publishDiagnostics": { "relatedInformation": true }
                }
            }
        });

        let init_result = self.send_request("initialize", params).await?;
        self.send_notification("initialized", json!({})).await?;
        Ok(init_result)
    }

    /// 打开文档通知（通知语言服务器载入最新文件内容）
    pub async fn did_open(&self, path: &Path, content: &str, language_id: &str) -> Result<(), String> {
        let uri = path_to_uri(path);
        let params = json!({
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": content,
            }
        });
        self.send_notification("textDocument/didOpen", params).await
    }

    /// 查询定义
    pub async fn definition(&self, path: &Path, line: u32, character: u32) -> Result<Vec<LspLocation>, String> {
        let (l0, c0) = LspPosition::new(line, character).to_lsp_zero_based();
        let uri = path_to_uri(path);

        let params = json!({
            "textDocument": { "uri": &uri },
            "position": { "line": l0, "character": c0 }
        });

        let result = self.send_request("textDocument/definition", params).await?;
        Self::parse_locations(result)
    }

    /// 查询全项目引用
    pub async fn references(&self, path: &Path, line: u32, character: u32, include_decl: bool) -> Result<Vec<LspLocation>, String> {
        let (l0, c0) = LspPosition::new(line, character).to_lsp_zero_based();
        let uri = path_to_uri(path);

        let params = json!({
            "textDocument": { "uri": &uri },
            "position": { "line": l0, "character": c0 },
            "context": { "includeDeclaration": include_decl }
        });

        let result = self.send_request("textDocument/references", params).await?;
        Self::parse_locations(result)
    }

    /// 诊断信息查询
    pub async fn diagnostics(&self, filter_path: Option<&Path>) -> Result<Vec<LspDiagnostic>, String> {
        let store = self.diagnostics_store.lock().await;
        if let Some(p) = filter_path {
            let uri = path_to_uri(p);
            Ok(store.get(&uri).cloned().unwrap_or_default())
        } else {
            let mut all = Vec::new();
            for diags in store.values() {
                all.extend(diags.iter().cloned());
            }
            Ok(all)
        }
    }

    /// 重命名符号
    pub async fn rename(&self, path: &Path, line: u32, character: u32, new_name: &str) -> Result<Value, String> {
        let (l0, c0) = LspPosition::new(line, character).to_lsp_zero_based();
        let uri = path_to_uri(path);

        let params = json!({
            "textDocument": { "uri": &uri },
            "position": { "line": l0, "character": c0 },
            "newName": new_name
        });

        self.send_request("textDocument/rename", params).await
    }

    fn parse_locations(result: Value) -> Result<Vec<LspLocation>, String> {
        let mut locations = Vec::new();

        let items = if let Some(arr) = result.as_array() {
            arr.clone()
        } else if result.is_object() {
            vec![result]
        } else {
            return Ok(locations);
        };

        for item in items {
            // 支持 Location: { uri, range: { start: { line, character } } }
            // 与 LocationLink: { targetUri, targetRange: { start: ... } }
            let uri_str = item.get("uri")
                .or_else(|| item.get("targetUri"))
                .and_then(Value::as_str);

            let range_obj = item.get("range")
                .or_else(|| item.get("targetSelectionRange"))
                .or_else(|| item.get("targetRange"))
                .and_then(Value::as_object);

            if let (Some(uri), Some(range)) = (uri_str, range_obj) {
                if let Some(start) = range.get("start").and_then(Value::as_object) {
                    let l0 = start.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let c0 = start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let pos = LspPosition::from_lsp_zero_based(l0, c0);

                    let file_path = uri_to_path(uri)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|| uri.to_string());

                    locations.push(LspLocation::new(file_path, pos.line, pos.character, None));
                }
            }
        }

        Ok(locations)
    }

    /// 在 Mock 模式下注入诊断结果
    pub async fn inject_diagnostics(&self, file_path: &Path, diagnostics: Vec<LspDiagnostic>) {
        let uri = path_to_uri(file_path);
        let mut store = self.diagnostics_store.lock().await;
        store.insert(uri, diagnostics);
    }
}
