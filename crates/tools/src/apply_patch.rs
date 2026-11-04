use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

use crate::jail::{display_relative, resolve};
use crate::write_file::MAX_WRITE_BYTES;
use crate::{required_str, Tool, ToolContext};

/// 单次补丁里的替换个数上限。再多就不像"改几行"，而像"整份重写"了——
/// 那种情况用 `write_file` 更诚实。
pub const MAX_EDITS: usize = 50;

struct Edit {
    old: String,
    new: String,
    /// 允许替换全部命中。默认只允许唯一命中，见下面的说明。
    all: bool,
}

pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "apply_patch",
            "对工作区内的文本文件做精确替换：给出原文片段 old 和新片段 new，只替换匹配到的部分。\
             默认要求 old 在文件里**只出现一次**——出现多次说明上下文不够，应补充更多行再试。\
             改之前先 read_file 确认原文。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "相对工作区的文件路径" },
                    "edits": {
                        "type": "array",
                        "description": "按顺序应用的替换列表",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old": { "type": "string", "description": "要被替换的原文，建议带 2–3 行上下文" },
                                "new": { "type": "string", "description": "替换成什么" },
                                "all": { "type": "boolean", "description": "允许替换全部命中，默认 false" }
                            },
                            "required": ["old", "new"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
            ToolKind::Write,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let raw = required_str(&input, "path")?;
        let path = resolve(&ctx.workspace, raw)?;
        let edits = parse_edits(&input)?;

        if path.is_dir() {
            return Err(Error::Tool(format!("{raw} 是目录，不能打补丁")));
        }
        let original = tokio::fs::read_to_string(&path).await.map_err(|e| {
            Error::Tool(format!("读取 {raw} 失败: {e}（新文件请用 write_file）"))
        })?;
        let original_bytes = original.len();

        let mut text = original;
        let mut applied = 0usize;

        for (index, edit) in edits.iter().enumerate() {
            let position = index + 1;
            let hits = text.matches(edit.old.as_str()).count();

            if hits == 0 {
                return Err(Error::Tool(format!(
                    "第 {position} 处替换在 {raw} 里找不到原文（第一行：{}）。\
                     文件可能已经改过，请重新 read_file 确认。",
                    first_line(&edit.old)
                )));
            }

            if hits > 1 && !edit.all {
                return Err(Error::Tool(format!(
                    "第 {position} 处替换的原文在 {raw} 里出现了 {hits} 次，无法确定改哪一处。\
                     请多带几行上下文，或在末尾用 all:true 明确要求全部替换。"
                )));
            }

            if edit.all {
                text = text.replace(edit.old.as_str(), edit.new.as_str());
                applied += hits;
            } else {
                text = text.replacen(edit.old.as_str(), edit.new.as_str(), 1);
                applied += 1;
            }
        }

        if text.len() > MAX_WRITE_BYTES {
            return Err(Error::Tool(format!(
                "改完有 {} 字节，超过 {} 字节上限",
                text.len(),
                MAX_WRITE_BYTES
            )));
        }

        tokio::fs::write(&path, &text)
            .await
            .map_err(|e| Error::Tool(format!("写入 {raw} 失败: {e}")))?;

        Ok(format!(
            "已修改 {}：{} 处替换（{} → {} 字节）",
            display_relative(&ctx.workspace, &path),
            applied,
            original_bytes,
            text.len()
        ))
    }
}

fn parse_edits(input: &Value) -> Result<Vec<Edit>> {
    let list = input
        .get("edits")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .ok_or_else(|| Error::Tool("缺少参数 edits（应为非空数组）".into()))?;

    if list.len() > MAX_EDITS {
        return Err(Error::Tool(format!(
            "一次最多 {MAX_EDITS} 处替换，给了 {} 处——这种规模请用 write_file",
            list.len()
        )));
    }

    let mut edits = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let position = index + 1;
        let old = item
            .get("old")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Tool(format!("第 {position} 处替换缺少 old（应为非空字符串）")))?;
        let new = item
            .get("new")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool(format!("第 {position} 处替换缺少 new（应为字符串）")))?;

        edits.push(Edit {
            old: old.to_string(),
            new: new.to_string(),
            all: item.get("all").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    Ok(edits)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_empty_and_oversized() {
        assert!(parse_edits(&json!({})).is_err());
        assert!(parse_edits(&json!({"edits": []})).is_err());
        assert!(parse_edits(&json!({"edits": [{"old": "", "new": "x"}]})).is_err());
        assert!(parse_edits(&json!({"edits": [{"old": "a"}]})).is_err());

        let many: Vec<Value> = (0..MAX_EDITS + 1)
            .map(|i| json!({"old": format!("a{i}"), "new": "b"}))
            .collect();
        assert!(parse_edits(&json!({"edits": many})).is_err());
    }

    #[test]
    fn parse_accepts_minimal_edit() {
        let edits = parse_edits(&json!({"edits": [{"old": "a", "new": "b"}]})).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old, "a");
        assert!(!edits[0].all, "默认必须是唯一命中模式");
    }

    #[test]
    fn first_line_helps_when_old_is_multiline() {
        assert_eq!(first_line("  fn main() {\n    body\n  }"), "fn main() {");
    }
}
