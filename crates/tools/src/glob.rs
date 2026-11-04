use async_trait::async_trait;
use goldenagent_core::{Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

use crate::jail::resolve;
use crate::walk::walk_files;
use crate::{Tool, ToolContext};

/// 单次返回的文件数上限。
pub const MAX_MATCHES: usize = 200;

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "glob",
            "按通配符列出工作区内的文件路径。支持 *（不跨目录）、**（跨任意层）、?（单个字符）。\
             模式里不含 / 时只匹配文件名，所以 *.rs 就能找到所有层级的 .rs。",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "通配模式，如 **/*.rs 或 src/*.ts" },
                    "path": { "type": "string", "description": "从哪个目录开始找，默认工作区根" },
                    "limit": { "type": "integer", "description": "最多返回多少个文件，默认 200" }
                },
                "required": ["pattern"]
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let pattern = crate::required_str(&input, "pattern")?;
        let start = resolve(
            &ctx.workspace,
            input.get("path").and_then(Value::as_str).unwrap_or("."),
        )?;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(MAX_MATCHES)
            .clamp(1, 1000);

        let walk = walk_files(&ctx.workspace, &start).await?;

        let mut found = Vec::new();
        for file in &walk.files {
            if !matches(pattern, &file.relative) {
                continue;
            }
            if found.len() >= limit {
                break;
            }
            found.push(file.relative.clone());
        }

        if found.is_empty() {
            return Ok(format!(
                "没有匹配「{pattern}」的文件（在 {} 下看了 {} 个文件{}）",
                start
                    .strip_prefix(&ctx.workspace)
                    .unwrap_or(&start)
                    .display(),
                walk.files.len(),
                if walk.truncated {
                    "，目录过多已提前停止遍历"
                } else {
                    ""
                }
            ));
        }

        let mut out = format!("{} 个文件\n", found.len());
        out.push_str(&found.join("\n"));
        if found.len() >= limit {
            out.push_str(&format!("\n…（已达 {limit} 个上限，请收窄模式）"));
        } else if walk.truncated {
            out.push_str("\n…（目录过多已提前停止遍历）");
        }
        Ok(out)
    }
}

/// 通配匹配入口。
///
/// 语义刻意和 `.gitignore` 一致：**模式里不含 `/` 时只比文件名**。
/// 否则用户写 `*.rs` 只能匹配根目录下的 `.rs`，十次里有九次会以为工具坏了。
pub fn matches(pattern: &str, relative: &str) -> bool {
    let pattern = pattern.trim().replace('\\', "/");
    if pattern.is_empty() {
        return false;
    }

    if !pattern.contains('/') {
        let name = relative.rsplit('/').next().unwrap_or(relative);
        return wildcard(&pattern, name);
    }

    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = relative.split('/').collect();
    match_segments(&pat, &segs)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    let Some((head, rest)) = pattern.split_first() else {
        return path.is_empty();
    };

    if *head == "**" {
        // `**` 能吃掉 0 到 n 段。0 段这一支保证 `a/**/b` 也能匹配 `a/b`。
        return (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]));
    }

    match path.split_first() {
        Some((first, tail)) => wildcard(head, first) && match_segments(rest, tail),
        None => false,
    }
}

/// 经典的双指针回溯通配：`*` 任意串、`?` 单字符。
///
/// 不用正则也不递归，是为了不让 `*a*a*a*a*b` 这种模式退化成指数级。
fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();

    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut resume = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(at) = star {
            // 上一次的 `*` 多吃一个字符再试
            pi = at + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_patterns_match_any_depth() {
        assert!(matches("*.rs", "src/main.rs"));
        assert!(matches("*.rs", "a/b/c/d.rs"));
        assert!(!matches("*.rs", "src/main.ts"));
    }

    #[test]
    fn full_path_patterns_are_anchored() {
        assert!(matches("src/*.rs", "src/main.rs"));
        assert!(!matches("src/*.rs", "src/deep/main.rs"));
        assert!(matches("src/**/*.rs", "src/deep/main.rs"));
        // `**` 也要匹配 0 层
        assert!(matches("src/**/*.rs", "src/main.rs"));
    }

    #[test]
    fn double_star_at_both_ends() {
        assert!(matches("**/test_*.rs", "crates/a/tests/test_x.rs"));
        assert!(matches("**/*.md", "README.md"));
        assert!(matches("src/**", "src/a/b/c.txt"));
    }

    #[test]
    fn question_mark_is_one_character() {
        assert!(matches("a?.txt", "ab.txt"));
        assert!(!matches("a?.txt", "abc.txt"));
        // 中文也算一个字符
        assert!(matches("第?章.md", "第一章.md"));
    }

    #[test]
    fn star_does_not_cross_separator() {
        // 反例：如果 `*` 跨目录，下面这条会被误判成命中
        assert!(!matches("src/*.rs", "src/a/b.rs"));
    }

    #[test]
    fn trailing_star_matches_rest() {
        assert!(wildcard("a*", "abc"));
        assert!(wildcard("*", ""));
        assert!(wildcard("*a*a*a*b", "aaaaaaab"));
        assert!(!wildcard("*a*a*a*b", "aaaaaaa"));
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        assert!(!matches("", "a.rs"));
        assert!(!matches("   ", "a.rs"));
    }
}
