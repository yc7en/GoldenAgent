use std::collections::HashMap;
use std::path::{Path, PathBuf};
use async_trait::async_trait;
use serde_json::{json, Value};
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};

use crate::jail::resolve_in;
use crate::{Tool, ToolContext};
use super::client::uri_to_path;

#[derive(Debug, Clone)]
pub struct RawTextEdit {
    pub start_line: usize,
    pub start_char: usize,
    pub end_line: usize,
    pub end_char: usize,
    pub new_text: String,
}

pub fn position_to_byte_offset(content: &str, line: usize, character: usize) -> Option<usize> {
    let mut current_line = 0;
    let mut current_offset = 0;

    for l in content.split_inclusive('\n') {
        if current_line == line {
            let mut char_count = 0;
            for (idx, ch) in l.char_indices() {
                if char_count == character {
                    return Some(current_offset + idx);
                }
                char_count += ch.len_utf16();
            }
            let end_without_nl = l.trim_end_matches(['\r', '\n']);
            let sub = if character < end_without_nl.len() { character } else { end_without_nl.len() };
            return Some(current_offset + sub);
        }
        current_offset += l.len();
        current_line += 1;
    }

    if current_line == line {
        return Some(current_offset);
    }
    None
}

pub fn apply_text_edits(content: &str, edits: &[RawTextEdit]) -> std::result::Result<String, String> {
    let mut offset_edits = Vec::new();
    for edit in edits {
        let start = position_to_byte_offset(content, edit.start_line, edit.start_char)
            .ok_or_else(|| format!("无效的编辑起始坐标: {}:{}", edit.start_line, edit.start_char))?;
        let end = position_to_byte_offset(content, edit.end_line, edit.end_char)
            .ok_or_else(|| format!("无效的编辑结束坐标: {}:{}", edit.end_line, edit.end_char))?;
        if start > end || end > content.len() {
            return Err("编辑区间越界".to_string());
        }
        offset_edits.push((start, end, edit.new_text.clone()));
    }

    // 倒序应用（从文件底部向顶部修改，保持前方偏移量有效）
    offset_edits.sort_by(|a, b| b.0.cmp(&a.0));

    let mut result = content.to_string();
    for (start, end, text) in offset_edits {
        result.replace_range(start..end, &text);
    }
    Ok(result)
}

fn extract_path(input: &Value) -> Option<&str> {
    input.get("path")
        .or_else(|| input.get("file"))
        .and_then(Value::as_str)
}

// ---------------------------------------------------------------------------
// 1. LspDefinition
// ---------------------------------------------------------------------------

pub struct LspDefinition;

#[async_trait]
impl Tool for LspDefinition {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "lsp_definition",
            "查询符号定义位置（跨文件跳转）。基于语言服务器（LSP）提供精确的定义文件路径与行列坐标。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件相对或绝对路径（也可传 file）" },
                    "file": { "type": "string", "description": "同 path" },
                    "line": { "type": "integer", "description": "光标所在行号（1-based）" },
                    "character": { "type": "integer", "description": "光标所在列号（1-based）" },
                    "summary": { "type": "string", "description": "操作意图说明（4-8字，如'查找类型定义'）" }
                },
                "required": ["line", "character"]
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let raw_path = extract_path(&input).ok_or_else(|| Error::Tool("缺少 path 或 file 参数".into()))?;
        let line = input.get("line").and_then(Value::as_u64).ok_or_else(|| Error::Tool("缺少 line 参数".into()))? as u32;
        let character = input.get("character").and_then(Value::as_u64).ok_or_else(|| Error::Tool("缺少 character 参数".into()))? as u32;

        let resolved = resolve_in(&ctx.roots(), raw_path)?;
        let client = ctx.lsp.get_or_spawn(&resolved, &ctx.workspace).await.map_err(Error::Tool)?;

        // 尝试打开最新内容
        if let Ok(content) = tokio::fs::read_to_string(&resolved).await {
            let lang = super::manager::LspManager::detect_language(&resolved).unwrap_or("plaintext");
            let _ = client.did_open(&resolved, &content, lang).await;
        }

        let locs = client.definition(&resolved, line, character).await.map_err(Error::Tool)?;
        if locs.is_empty() {
            return Ok("未在指定位置找到符号定义".to_string());
        }

        let mut lines = Vec::new();
        lines.push(format!("找到 {} 处定义：", locs.len()));
        for loc in locs {
            let rel = Path::new(&loc.file)
                .strip_prefix(&ctx.workspace)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| loc.file.clone());

            let mut preview = String::new();
            if let Ok(file_str) = tokio::fs::read_to_string(&loc.file).await {
                if let Some(target_line) = file_str.lines().nth(loc.line.saturating_sub(1) as usize) {
                    preview = format!("\n  > {}", target_line.trim());
                }
            }
            lines.push(format!("- {}:{}:{}{}", rel, loc.line, loc.character, preview));
        }

        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// 2. LspReferences
// ---------------------------------------------------------------------------

pub struct LspReferences;

#[async_trait]
impl Tool for LspReferences {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "lsp_references",
            "查询符号在全项目中的所有引用位置。支持跨文件查找函数、类型、变量或方法的调用点。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件相对或绝对路径（也可传 file）" },
                    "file": { "type": "string", "description": "同 path" },
                    "line": { "type": "integer", "description": "光标所在行号（1-based）" },
                    "character": { "type": "integer", "description": "光标所在列号（1-based）" },
                    "include_declaration": { "type": "boolean", "description": "是否包含定义本身（默认 true）" },
                    "summary": { "type": "string", "description": "操作意图说明（4-8字，如'查找符号引用'）" }
                },
                "required": ["line", "character"]
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let raw_path = extract_path(&input).ok_or_else(|| Error::Tool("缺少 path 或 file 参数".into()))?;
        let line = input.get("line").and_then(Value::as_u64).ok_or_else(|| Error::Tool("缺少 line 参数".into()))? as u32;
        let character = input.get("character").and_then(Value::as_u64).ok_or_else(|| Error::Tool("缺少 character 参数".into()))? as u32;
        let include_decl = input.get("include_declaration").and_then(Value::as_bool).unwrap_or(true);

        let resolved = resolve_in(&ctx.roots(), raw_path)?;
        let client = ctx.lsp.get_or_spawn(&resolved, &ctx.workspace).await.map_err(Error::Tool)?;

        if let Ok(content) = tokio::fs::read_to_string(&resolved).await {
            let lang = super::manager::LspManager::detect_language(&resolved).unwrap_or("plaintext");
            let _ = client.did_open(&resolved, &content, lang).await;
        }

        let locs = client.references(&resolved, line, character, include_decl).await.map_err(Error::Tool)?;
        if locs.is_empty() {
            return Ok("未在指定位置找到任何符号引用".to_string());
        }

        let mut lines = Vec::new();
        lines.push(format!("共找到 {} 处引用：", locs.len()));
        for loc in locs {
            let rel = Path::new(&loc.file)
                .strip_prefix(&ctx.workspace)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| loc.file.clone());

            let mut preview = String::new();
            if let Ok(file_str) = tokio::fs::read_to_string(&loc.file).await {
                if let Some(target_line) = file_str.lines().nth(loc.line.saturating_sub(1) as usize) {
                    preview = format!(" -> {}", target_line.trim());
                }
            }
            lines.push(format!("- {}:{}:{}{}", rel, loc.line, loc.character, preview));
        }

        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// 3. LspDiagnostics
// ---------------------------------------------------------------------------

pub struct LspDiagnostics;

#[async_trait]
impl Tool for LspDiagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "lsp_diagnostics",
            "获取当前工作区或指定文件的实时编译器诊断（错误与警告），无需全量编译构建即可自检代码质量。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "可选。指定文件路径；若不提供则返回整个工作区的全部诊断" },
                    "file": { "type": "string", "description": "同 path" },
                    "summary": { "type": "string", "description": "操作意图说明（4-8字，如'查看编译告警'）" }
                }
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let opt_raw = extract_path(&input);
        let filter_path = if let Some(raw) = opt_raw {
            Some(resolve_in(&ctx.roots(), raw)?)
        } else {
            None
        };

        let target_for_client = filter_path.as_deref().unwrap_or(&ctx.workspace);
        // 如果文件有扩展名则按扩展名匹配，否则尝试通用 rust 文件
        let probe = if filter_path.is_some() {
            target_for_client.to_path_buf()
        } else {
            ctx.workspace.join("main.rs")
        };

        let client = match ctx.lsp.get_or_spawn(&probe, &ctx.workspace).await {
            Ok(c) => c,
            Err(e) => return Err(Error::Tool(e)),
        };

        let diags = client.diagnostics(filter_path.as_deref()).await.map_err(Error::Tool)?;
        if diags.is_empty() {
            return Ok("代码自检通过：未发现语法或类型错误/警告".to_string());
        }

        let mut lines = Vec::new();
        lines.push(format!("发现 {} 条编译器诊断信息：", diags.len()));
        for diag in diags {
            let rel = Path::new(&diag.file)
                .strip_prefix(&ctx.workspace)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| diag.file.clone());

            let code_str = diag.code.as_deref().map(|c| format!(" [{c}]")).unwrap_or_default();
            lines.push(format!(
                "- [{}] {}:{}:{}{}: {}",
                diag.severity, rel, diag.line, diag.character, code_str, diag.message
            ));
        }

        Ok(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// 4. LspRename
// ---------------------------------------------------------------------------

pub struct LspRename;

#[async_trait]
impl Tool for LspRename {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "lsp_rename",
            "在全项目中安全重命名符号（变量、函数、类名等）。自动计算所有引用点并跨文件原子修改代码。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "符号所在文件相对或绝对路径（也可传 file）" },
                    "file": { "type": "string", "description": "同 path" },
                    "line": { "type": "integer", "description": "光标所在行号（1-based）" },
                    "character": { "type": "integer", "description": "光标所在列号（1-based）" },
                    "new_name": { "type": "string", "description": "修改后的新符号名称" },
                    "summary": { "type": "string", "description": "操作意图说明（4-8字，如'安全重命名函数'）" }
                },
                "required": ["line", "character", "new_name"]
            }),
            ToolKind::Write,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let raw_path = extract_path(&input).ok_or_else(|| Error::Tool("缺少 path 或 file 参数".into()))?;
        let line = input.get("line").and_then(Value::as_u64).ok_or_else(|| Error::Tool("缺少 line 参数".into()))? as u32;
        let character = input.get("character").and_then(Value::as_u64).ok_or_else(|| Error::Tool("缺少 character 参数".into()))? as u32;
        let new_name = input.get("new_name").and_then(Value::as_str).ok_or_else(|| Error::Tool("缺少 new_name 参数".into()))?;

        let resolved = resolve_in(&ctx.roots(), raw_path)?;
        let client = ctx.lsp.get_or_spawn(&resolved, &ctx.workspace).await.map_err(Error::Tool)?;

        if let Ok(content) = tokio::fs::read_to_string(&resolved).await {
            let lang = super::manager::LspManager::detect_language(&resolved).unwrap_or("plaintext");
            let _ = client.did_open(&resolved, &content, lang).await;
        }

        let resp = client.rename(&resolved, line, character, new_name).await.map_err(Error::Tool)?;

        // 解析 WorkspaceEdit: changes 或 documentChanges
        let mut file_edits: HashMap<PathBuf, Vec<RawTextEdit>> = HashMap::new();

        if let Some(changes) = resp.get("changes").and_then(Value::as_object) {
            for (uri, edits_val) in changes {
                let Some(path) = uri_to_path(uri) else { continue };
                let target = resolve_in(&ctx.roots(), &path.to_string_lossy())?;
                if let Some(edits_arr) = edits_val.as_array() {
                    for e in edits_arr {
                        if let Some(raw_edit) = Self::parse_edit(e) {
                            file_edits.entry(target.clone()).or_default().push(raw_edit);
                        }
                    }
                }
            }
        } else if let Some(doc_changes) = resp.get("documentChanges").and_then(Value::as_array) {
            for dc in doc_changes {
                if let Some(uri) = dc.get("textDocument").and_then(|td| td.get("uri")).and_then(Value::as_str) {
                    let Some(path) = uri_to_path(uri) else { continue };
                    let target = resolve_in(&ctx.roots(), &path.to_string_lossy())?;
                    if let Some(edits_arr) = dc.get("edits").and_then(Value::as_array) {
                        for e in edits_arr {
                            if let Some(raw_edit) = Self::parse_edit(e) {
                                file_edits.entry(target.clone()).or_default().push(raw_edit);
                            }
                        }
                    }
                }
            }
        }

        if file_edits.is_empty() {
            return Ok(format!("语言服务器未返回针对符号 '{new_name}' 的有效修改点"));
        }

        // 统一在内存中计算并应用，若任一文件计算失败则中止写入
        let mut modified_contents: Vec<(PathBuf, String, usize)> = Vec::new();
        let mut total_edits = 0;

        for (file_path, edits) in &file_edits {
            let content = tokio::fs::read_to_string(file_path).await
                .map_err(|e| Error::Tool(format!("读取目标文件 {} 失败: {e}", file_path.display())))?;
            let new_content = apply_text_edits(&content, edits)
                .map_err(|e| Error::Tool(format!("应用代码替换失败 ({}): {e}", file_path.display())))?;
            total_edits += edits.len();
            modified_contents.push((file_path.clone(), new_content, edits.len()));
        }

        // 落地写入文件
        for (file_path, new_content, _) in &modified_contents {
            tokio::fs::write(file_path, new_content).await
                .map_err(|e| Error::Tool(format!("写入目标文件 {} 失败: {e}", file_path.display())))?;
        }

        let mut summary = Vec::new();
        summary.push(format!(
            "成功将符号重命名为 '{}'，共涉及 {} 个文件，应用了 {} 处修改：",
            new_name,
            modified_contents.len(),
            total_edits
        ));

        for (file_path, _, count) in &modified_contents {
            let rel = file_path.strip_prefix(&ctx.workspace)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| file_path.to_string_lossy().to_string());
            summary.push(format!("- {} ({} 处修改)", rel, count));
        }

        Ok(summary.join("\n"))
    }
}

impl LspRename {
    fn parse_edit(e: &Value) -> Option<RawTextEdit> {
        let range = e.get("range")?.as_object()?;
        let start = range.get("start")?.as_object()?;
        let end = range.get("end")?.as_object()?;

        let s_l = start.get("line")?.as_u64()? as usize;
        let s_c = start.get("character")?.as_u64()? as usize;
        let e_l = end.get("line")?.as_u64()? as usize;
        let e_c = end.get("character")?.as_u64()? as usize;
        let new_text = e.get("newText")?.as_str()?.to_string();

        Some(RawTextEdit {
            start_line: s_l,
            start_char: s_c,
            end_line: e_l,
            end_char: e_c,
            new_text,
        })
    }
}
