use serde::{Deserialize, Serialize};

/// LSP 行列位置（1-based 供对外与模型交互使用，内部可与 0-based 互相转换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

impl LspPosition {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    /// 转换为 LSP 协议标准的 0-based 行列号。
    pub fn to_lsp_zero_based(self) -> (u32, u32) {
        (
            self.line.saturating_sub(1),
            self.character.saturating_sub(1),
        )
    }

    /// 从 LSP 协议的 0-based 行列号构造 1-based 位置。
    pub fn from_lsp_zero_based(line: u32, character: u32) -> Self {
        Self {
            line: line.saturating_add(1),
            character: character.saturating_add(1),
        }
    }
}

/// LSP 代码位置引用（文件路径 + 行列坐标 + 可选单行代码预览）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspLocation {
    pub file: String,
    pub line: u32,
    pub character: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

impl LspLocation {
    pub fn new(file: impl Into<String>, line: u32, character: u32, preview: Option<String>) -> Self {
        Self {
            file: file.into(),
            line,
            character,
            preview,
        }
    }
}

/// LSP 诊断信息（来自语言服务器的编译告警与类型报错）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspDiagnostic {
    pub file: String,
    pub line: u32,
    pub character: u32,
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl LspDiagnostic {
    pub fn new(
        file: impl Into<String>,
        line: u32,
        character: u32,
        severity: impl Into<String>,
        message: impl Into<String>,
        code: Option<String>,
    ) -> Self {
        Self {
            file: file.into(),
            line,
            character,
            severity: severity.into(),
            message: message.into(),
            code,
        }
    }
}
