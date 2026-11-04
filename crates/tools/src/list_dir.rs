use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

use crate::jail::{display_relative, resolve};
use crate::{Tool, ToolContext};

/// 单次列出的条目上限，防止在一个巨型目录（如 node_modules）里打转。
pub const MAX_ENTRIES: usize = 500;

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "list_dir",
            "列出工作区内某个目录的直接子项：目录名后带 /，文件带字节数。默认列工作区根目录。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "相对工作区的目录路径，默认 ." }
                }
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let raw = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".");
        let dir = resolve(&ctx.workspace, raw)?;

        let meta = tokio::fs::metadata(&dir)
            .await
            .map_err(|e| Error::Tool(format!("打开目录 {raw} 失败: {e}")))?;
        if !meta.is_dir() {
            return Err(Error::Tool(format!("{raw} 不是目录，请用 read_file")));
        }

        let mut names = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| Error::Tool(format!("读取目录 {raw} 失败: {e}")))?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| Error::Tool(format!("读取目录项失败: {e}")))?
        {
            if names.len() >= MAX_ENTRIES {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry
                .file_type()
                .await
                .map_err(|e| Error::Tool(format!("读取 {name} 类型失败: {e}")))?;

            if file_type.is_dir() {
                names.push(format!("{name}/"));
            } else {
                let size = entry
                    .metadata()
                    .await
                    .map(|m| m.len())
                    .unwrap_or_default();
                names.push(format!("{name}\t{size} B"));
            }
        }

        names.sort();
        let header = format!("{}（{} 项）\n", display_relative(&ctx.workspace, &dir), names.len());

        if names.is_empty() {
            return Ok(format!("{header}（空目录）"));
        }

        let mut out = header;
        out.push_str(&names.join("\n"));
        if names.len() >= MAX_ENTRIES {
            out.push_str(&format!("\n…（超过 {MAX_ENTRIES} 项已截断）"));
        }
        Ok(out)
    }
}
