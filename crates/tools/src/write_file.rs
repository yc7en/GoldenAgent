use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

use crate::jail::display_relative;
use crate::{required_str, Tool, ToolContext};

/// 单次写入的字节上限。防的是模型一次性吐出一份巨型文件把会话撑爆。
pub const MAX_WRITE_BYTES: usize = 1024 * 1024;

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write_file",
            "写入或覆盖工作区内的文件，父目录不存在会自动创建。单次上限 1 MiB。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "相对工作区的路径" },
                    "content": { "type": "string", "description": "完整文件内容（覆盖写）" },
                    "summary": { "type": "string", "description": "简要操作意图（4-8字），如'写入配置文件'" }
                },
                "required": ["path", "content"]
            }),
            ToolKind::Write,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let raw = required_str(&input, "path")?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("缺少参数 content".into()))?;

        if content.len() > MAX_WRITE_BYTES {
            return Err(Error::Tool(format!(
                "内容有 {} 字节，超过 {} 字节上限",
                content.len(),
                MAX_WRITE_BYTES
            )));
        }

        let path = ctx.resolve(raw)?;

        if path.is_dir() {
            return Err(Error::Tool(format!("{raw} 是目录，不能写入")));
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Tool(format!("创建目录 {} 失败: {e}", parent.display())))?;
        }

        let existed = path.exists();
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| Error::Tool(format!("写入 {raw} 失败: {e}")))?;

        let relative = display_relative(&ctx.workspace, &path);
        Ok(format!(
            "{} {}（{} 字节）",
            if existed { "已覆盖" } else { "已创建" },
            relative,
            content.len()
        ))
    }
}
