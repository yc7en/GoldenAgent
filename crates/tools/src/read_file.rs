use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};
use serde_json::{json, Value};


use crate::{required_str, Tool, ToolContext};

/// 单次读取的字节上限。超了直接拒绝，不截断——
/// 截断会让模型以为看到了全文，比报错更危险。
pub const MAX_READ_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_LIMIT: usize = 2000;

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read_file",
            "读取工作区内文本文件，返回带行号的内容。单文件上限 1 MiB，超过会报错。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "相对工作区的路径，如 src/main.rs" },
                    "offset": { "type": "integer", "description": "起始行号，从 1 开始，默认 1" },
                    "limit": { "type": "integer", "description": "最多返回多少行，默认 2000" }
                },
                "required": ["path"]
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let raw = required_str(&input, "path")?;
        let path = ctx.resolve(raw)?;

        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| Error::Tool(format!("读取 {raw} 失败: {e}")))?;

        if meta.is_dir() {
            return Err(Error::Tool(format!("{raw} 是目录，请用 list_dir")));
        }
        if meta.len() > MAX_READ_BYTES {
            return Err(Error::Tool(format!(
                "{raw} 有 {} 字节，超过 {} 字节上限，请改用 grep 或分段读取",
                meta.len(),
                MAX_READ_BYTES
            )));
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| Error::Tool(format!("{raw} 不是有效 UTF-8 或读取失败: {e}")))?;

        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .max(1);

        let total = content.lines().count();
        let mut out = String::new();
        for (index, line) in content.lines().enumerate() {
            let line_no = index + 1;
            if line_no < offset || line_no >= offset + limit {
                continue;
            }
            out.push_str(&format!("{line_no:>6}\t{line}\n"));
        }

        if out.is_empty() {
            return Ok(format!("（{raw} 共 {total} 行，第 {offset} 行之后没有内容）"));
        }

        let shown_end = (offset + limit - 1).min(total);
        if offset > 1 || shown_end < total {
            out.push_str(&format!(
                "\n（已显示第 {offset}–{shown_end} 行，全文共 {total} 行）\n"
            ));
        }
        Ok(out)
    }
}
