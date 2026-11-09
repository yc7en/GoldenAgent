use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::client::LspClient;

pub struct LspManager {
    clients: Mutex<HashMap<(String, PathBuf), Arc<LspClient>>>,
    mock_clients: Mutex<HashMap<String, Arc<LspClient>>>,
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspManager").finish()
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            mock_clients: Mutex::new(HashMap::new()),
        }
    }

    /// 注册测试用 Mock 客户端
    pub async fn register_mock_client(&self, language: impl Into<String>, client: Arc<LspClient>) {
        let mut mocks = self.mock_clients.lock().await;
        mocks.insert(language.into(), client);
    }

    /// 推导文件对应的语言标识（languageId）
    pub fn detect_language(path: &Path) -> Option<&'static str> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some("rust"),
            "ts" | "tsx" | "js" | "jsx" => Some("typescript"),
            "py" => Some("python"),
            "go" => Some("go"),
            "c" | "cpp" | "cc" | "h" | "hpp" => Some("cpp"),
            _ => None,
        }
    }

    /// 获取默认的语言服务器启动命令与参数
    pub fn default_server_for(language: &str) -> Option<(&'static str, &'static [&'static str])> {
        match language {
            "rust" => Some(("rust-analyzer", &[])),
            "typescript" => Some(("typescript-language-server", &["--stdio"])),
            "python" => Some(("pyright-langserver", &["--stdio"])),
            "go" => Some(("gopls", &[])),
            _ => None,
        }
    }

    /// 获取或启动对应文件语言的 LSP 客户端
    pub async fn get_or_spawn(&self, file_path: &Path, workspace: &Path) -> Result<Arc<LspClient>, String> {
        let language = Self::detect_language(file_path).ok_or_else(|| {
            format!(
                "无法识别文件 '{}' 的编程语言扩展名，未能匹配到支持的 LSP 语言服务器",
                file_path.display()
            )
        })?;

        // 优先检查是否有注入的 Mock 客户端
        {
            let mocks = self.mock_clients.lock().await;
            if let Some(client) = mocks.get(language).or_else(|| mocks.get("*")) {
                return Ok(Arc::clone(client));
            }
        }

        let key = (language.to_string(), workspace.to_path_buf());
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&key) {
            return Ok(Arc::clone(client));
        }

        let (cmd, args) = Self::default_server_for(language).ok_or_else(|| {
            format!("暂未配置语言 '{language}' 的默认 LSP 服务器启动命令")
        })?;

        let client = LspClient::spawn(cmd, args, workspace).await.map_err(|e| {
            format!(
                "未在系统 PATH 中找到语言服务器 '{cmd}'（{e}）。\
                 提示：请确认环境已安装并配置 PATH，或改用 grep / glob / read_file 等工具进行纯文本代码检索与编辑。"
            )
        })?;

        clients.insert(key, Arc::clone(&client));
        Ok(client)
    }
}
