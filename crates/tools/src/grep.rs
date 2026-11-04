use async_trait::async_trait;
use goldenagent_core::{Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

use crate::glob::matches;
use crate::jail::resolve;
use crate::walk::walk_files;
use crate::{required_str, Tool, ToolContext};

/// 单次返回的命中上限。再多模型也读不完，只会把上下文撑爆。
pub const MAX_HITS: usize = 200;

/// 参与搜索的单文件上限。超过就跳过并计数——
/// 一个几百 MB 的日志文件既搜不快，也几乎不会是模型要找的东西。
pub const MAX_GREP_BYTES: u64 = 4 * 1024 * 1024;

/// 单行展示上限。压缩过的 JS 可能一整份只有一行。
const MAX_LINE_CHARS: usize = 300;

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "grep",
            "在工作区里按字面文本搜索文件内容，返回 路径:行号: 内容。\
             不是正则——需要正则请先用字面片段缩小范围。默认跳过 .git / node_modules / target。",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "要找的字面文本，不是正则" },
                    "path": { "type": "string", "description": "从哪个目录或文件开始搜，默认工作区根" },
                    "glob": { "type": "string", "description": "只搜文件名匹配这个模式的文件，如 *.rs 或 **/*.md" },
                    "ignore_case": { "type": "boolean", "description": "是否忽略大小写，默认 false" },
                    "max_hits": { "type": "integer", "description": "最多返回多少处命中，默认 200" }
                },
                "required": ["pattern"]
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let pattern = required_str(&input, "pattern")?;
        let start = resolve(&ctx.workspace, input.get("path").and_then(Value::as_str).unwrap_or("."))?;
        let filter = input.get("glob").and_then(Value::as_str);
        let ignore_case = input
            .get("ignore_case")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let max_hits = input
            .get("max_hits")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(MAX_HITS)
            .clamp(1, 1000);

        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };

        let walk = walk_files(&ctx.workspace, &start).await?;

        let mut hits: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let mut skipped_large = 0usize;
        let mut skipped_binary = 0usize;
        let mut truncated = false;

        'files: for file in &walk.files {
            if let Some(filter) = filter {
                if !matches(filter, &file.relative) {
                    continue;
                }
            }
            if file.size > MAX_GREP_BYTES {
                skipped_large += 1;
                continue;
            }

            // 读不动或不是 UTF-8 就跳过：二进制文件里"搜到"的东西对模型没意义。
            let Ok(content) = tokio::fs::read_to_string(&file.path).await else {
                skipped_binary += 1;
                continue;
            };
            scanned += 1;

            for (index, line) in content.lines().enumerate() {
                let haystack = if ignore_case {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if !haystack.contains(&needle) {
                    continue;
                }

                if hits.len() >= max_hits {
                    truncated = true;
                    break 'files;
                }
                hits.push(format!(
                    "{}:{}:{}",
                    file.relative,
                    index + 1,
                    shorten(line.trim_end())
                ));
            }
        }

        if hits.is_empty() {
            return Ok(format!(
                "没有匹配「{pattern}」（扫描 {scanned} 个文件{}）",
                sweep_note(walk.truncated, skipped_large, skipped_binary)
            ));
        }

        let mut out = format!(
            "{} 处命中（扫描 {scanned} 个文件）\n",
            hits.len()
        );
        out.push_str(&hits.join("\n"));
        if truncated {
            out.push_str(&format!("\n…（已达 {max_hits} 处上限，后面可能还有）"));
        }
        let note = sweep_note(walk.truncated, skipped_large, skipped_binary);
        if !note.is_empty() {
            out.push_str(&format!("\n{note}"));
        }
        Ok(out)
    }
}

/// 把"这次搜索漏了什么"说清楚。漏了却不说，等于给了个假答案。
fn sweep_note(walk_truncated: bool, large: usize, binary: usize) -> String {
    let mut parts = Vec::new();
    if walk_truncated {
        parts.push("目录过多已提前停止遍历".to_string());
    }
    if large > 0 {
        parts.push(format!("{large} 个超过 4 MiB 的文件未搜索"));
    }
    if binary > 0 {
        parts.push(format!("{binary} 个非文本文件未搜索"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("（{}）", parts.join("；"))
    }
}

fn shorten(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_are_shortened() {
        let line = "字".repeat(MAX_LINE_CHARS + 10);
        let short = shorten(&line);
        assert!(short.ends_with('…'));
        assert_eq!(short.chars().count(), MAX_LINE_CHARS + 1);
        assert_eq!(shorten("短"), "短");
    }

    #[test]
    fn sweep_note_names_what_was_skipped() {
        assert_eq!(sweep_note(false, 0, 0), "");
        let note = sweep_note(true, 2, 1);
        assert!(note.contains("提前停止"), "{note}");
        assert!(note.contains("2 个"), "{note}");
        assert!(note.contains("1 个"), "{note}");
    }
}
