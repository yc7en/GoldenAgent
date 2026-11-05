//! 技能（skills）：只读的指令包。
//!
//! 一个 skill = 一个目录 + 一份 `SKILL.md`。它**不带来新能力**，
//! 只是把「这件事该怎么做」从代码里搬到可替换的文件里——换一个领域不必改代码重新打包。
//!
//! 第一期只读：目录即安装，删掉即失效，不写任何状态文件。
//! 也正因为如此，这里没有"注册表"这种东西：目录名就是 id，磁盘就是唯一真源。
//!
//! 边界写在 `ARCHITECTURE.md` 的 A-14：`use_skill` 只能解析到该技能目录之内，
//! 且 skill 不改变任何权限裁决——正文里写的 `bash ...` 仍按当前档位走。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

use crate::{required_str, Tool, ToolContext};

pub const SKILL_FILE: &str = "SKILL.md";
/// 清单里最多列这么多个。再多，系统提示本身就该分页了。
pub const MAX_SKILLS: usize = 32;
/// description 是「一句话说清什么时候用它」，200 字已经是很宽的一句话。
pub const MAX_DESCRIPTION_CHARS: usize = 200;
/// 单个文件（`SKILL.md` 或它引用的 references）的读取上限。
pub const MAX_SKILL_BYTES: u64 = 64 * 1024;
/// 单次 `use_skill` 最多回多少行，与 `read_file` 同理：一次吐太多会挤掉别的上下文。
pub const DEFAULT_LINES: usize = 800;
/// 注入系统提示的清单总长上限（约 2K token）。
///
/// 32 个技能 × 200 字描述 ≈ 19 KB，比这个上限大——所以清单本身也要能被截断，
/// 并且截断之后必须说出来"还有几个没列"。
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;

/// frontmatter 只解析这么多字节。再长说明它把正文写进了元数据，那不是元数据。
const FRONTMATTER_LIMIT: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// 目录名。也是模型调 `use_skill` 时该传的值。
    pub id: String,
    /// 展示名：frontmatter 的 `name`，缺省等于 `id`。
    pub title: String,
    pub description: String,
}

impl Skill {
    /// 拼进系统提示的一行。标题与 id 相同时不重复写。
    pub fn outline(&self) -> String {
        if self.title == self.id {
            format!("- {}：{}", self.id, self.description)
        } else {
            format!("- {}（{}）：{}", self.id, self.title, self.description)
        }
    }
}

/// 没装进来的目录，以及为什么。
///
/// 不静默跳过是刻意的：用户放了一个 skill 却没生效时，
/// 「为什么没生效」比「有没有生效」更难查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub skills: Vec<Skill>,
    pub skipped: Vec<Skipped>,
}

/// 扫一遍技能根目录。
///
/// 用同步 IO 而不是 `tokio::fs`：它只在每轮对话前调用一次，读的是几十个几 KB 的文本，
/// 为这点事给每个文件排一次 `spawn_blocking` 反而更重。
///
/// 目录不存在**不是错误**——绝大多数用户根本没有这个目录，那时候扫描结果就该是空的。
pub fn scan(root: &Path) -> Scan {
    let mut report = Scan::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return report;
    };

    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        // `.` 开头的一律不看：那是用户的临时目录或我们的 `.DS_Store` 之类。
        if id.starts_with('.') {
            continue;
        }
        if !is_valid_id(&id) {
            report.skipped.push(Skipped {
                id,
                reason: format!("目录名不合法（只允许小写字母、数字、减号，最长 64）"),
            });
            continue;
        }
        dirs.push((id, path));
    }

    // 排序之后再截断：否则"哪 32 个进了清单"取决于文件系统的返回顺序，
    // 同一个目录在不同机器上会给出不同的提示词。
    dirs.sort_by(|a, b| a.0.cmp(&b.0));

    for (id, dir) in dirs {
        if report.skills.len() >= MAX_SKILLS {
            report.skipped.push(Skipped {
                id,
                reason: format!("超过 {MAX_SKILLS} 个上限，未列入清单"),
            });
            continue;
        }
        match load(&id, &dir) {
            Ok(skill) => report.skills.push(skill),
            Err(reason) => report.skipped.push(Skipped { id, reason }),
        }
    }

    report
}

fn load(id: &str, dir: &Path) -> std::result::Result<Skill, String> {
    let file = dir.join(SKILL_FILE);
    let meta = std::fs::metadata(&file).map_err(|_| format!("没有 {SKILL_FILE}"))?;
    if meta.len() > MAX_SKILL_BYTES {
        return Err(format!(
            "{SKILL_FILE} 有 {} 字节，超过 {MAX_SKILL_BYTES} 字节上限",
            meta.len()
        ));
    }
    let bytes = std::fs::read(&file).map_err(|e| format!("读不了 {SKILL_FILE}: {e}"))?;
    // 走同一套解码：用户的 skill 可能是从别处拷来的 GBK 文本。
    let text = crate::encoding::decode(&bytes);

    let meta = parse_meta(&text);
    let description = meta.description.as_deref().unwrap_or("").trim().to_string();
    if description.is_empty() {
        return Err(format!("{SKILL_FILE} 的 frontmatter 缺少 description"));
    }

    Ok(Skill {
        id: id.to_string(),
        title: meta.name.unwrap_or_else(|| id.to_string()).trim().to_string(),
        description: clip_chars(&description, MAX_DESCRIPTION_CHARS),
    })
}

/// 目录名白名单。
///
/// 比"禁止 `..` 和分隔符"更省事：白名单之外的一律不认，路径穿越就没有落点。
fn is_valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !id.starts_with('-')
}

#[derive(Debug, Default)]
struct Meta {
    name: Option<String>,
    description: Option<String>,
}

/// 极简 frontmatter 解析：只认单行的 `key: value`。
///
/// 刻意不引 YAML 库——我们只要两个字段，而 YAML 的类型系统带来的问题
/// （锚点、多行折叠、`yes` 是不是布尔）远多于它能解决的。写不出来的形态就明确不支持：
/// description 用 `>` / `|` 开头的，直接当"缺 description"处理，报错里说清。
fn parse_meta(text: &str) -> Meta {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---") else {
        return Meta::default();
    };
    // `---` 必须单独成行，否则 `----` 这种分隔线会被误认成 frontmatter 开头。
    let Some(rest) = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n")) else {
        return Meta::default();
    };

    let mut meta = Meta::default();
    let mut consumed = 0usize;
    for line in rest.lines() {
        consumed += line.len() + 1;
        if consumed > FRONTMATTER_LIMIT {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed == "---" || trimmed == "..." {
            break;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // 多行写法不支持：留着半个 `>` 当描述比明确失败更糟糕。
        if value.starts_with('>') || value.starts_with('|') {
            continue;
        }
        let value = unquote(value);
        match key.trim() {
            "name" if meta.name.is_none() => meta.name = Some(value),
            "description" if meta.description.is_none() => meta.description = Some(value),
            _ => {}
        }
    }
    meta
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            let inner = &value[1..value.len() - 1];
            return inner.replace("\\\"", "\"").replace("\\\\", "\\");
        }
    }
    value.to_string()
}

fn clip_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// 现有技能 id，给报错用。
///
/// 模型猜错名字是最常见的失败，报错里直接把可用列表给它，比让它再调一次 `list_dir` 快。
fn available_ids(root: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|id| !id.starts_with('.') && is_valid_id(id))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

pub struct UseSkill;

#[async_trait]
impl Tool for UseSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "use_skill",
            "读取一个技能的说明文档。技能是用户放进 skills 目录的指令包，\
             系统提示里会列出可用的技能名。调用前不需要先看别的，直接按名字读；\
             文档里若引用了 references/ 下的文件，可以再用 path 参数读它们。",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能名，即 skills 目录下的文件夹名" },
                    "path": { "type": "string", "description": "技能目录内的相对路径，默认 SKILL.md" },
                    "offset": { "type": "integer", "description": "起始行号，从 1 开始，默认 1" },
                    "limit": { "type": "integer", "description": "最多返回多少行，默认 800" }
                },
                "required": ["name"]
            }),
            // 读类：技能只是文本，读它不该比读工作区文件更严格。
            ToolKind::Read,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let root = ctx.skills_root.as_ref().ok_or_else(|| {
            Error::Tool("当前没有可用的技能目录（skills）。技能需要放在工作区的 .goldenagent/skills/ 下".into())
        })?;

        let id = required_str(&input, "name")?;
        if !is_valid_id(id) {
            return Err(Error::Tool(format!(
                "技能名 {id} 不合法：只允许小写字母、数字与减号"
            )));
        }

        // 先按目录名拼出物理路径，再用 jail 单根判定——
        // 这样"能读到什么"由目录结构决定，不由模型给的字符串决定（A-14）。
        let dir = root.join(id);
        if !dir.is_dir() {
            let available = available_ids(root);
            return Err(Error::Tool(if available.is_empty() {
                format!(
                    "没有名为 {id} 的技能，该目录下一个技能也没有（{}）",
                    root.display()
                )
            } else {
                format!("没有名为 {id} 的技能，现有的是：{}", available.join("、"))
            }));
        }

        let raw = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(SKILL_FILE);

        let path = crate::jail::resolve_in(std::slice::from_ref(&dir), raw).map_err(|_| {
            Error::Tool(format!(
                "技能 {id} 里不允许访问 {raw}：只能读该技能目录内的文件"
            ))
        })?;

        let meta = tokio::fs::metadata(&path).await.map_err(|_| {
            Error::Tool(format!(
                "技能 {id} 里没有 {raw}。可以先用 path 指向目录看它有哪些文件"
            ))
        })?;

        if meta.is_dir() {
            return list_files(&id, raw, &path).await;
        }
        if meta.len() > MAX_SKILL_BYTES {
            return Err(Error::Tool(format!(
                "技能文件 {id}/{raw} 有 {} 字节，超过 {MAX_SKILL_BYTES} 字节上限",
                meta.len()
            )));
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| Error::Tool(format!("读取 {id}/{raw} 失败: {e}")))?;
        let text = crate::encoding::decode(&bytes);

        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LINES)
            .max(1);

        let total = text.lines().count();
        let mut out = format!("技能 {id} · {raw} · 共 {total} 行\n");
        let mut shown_end = offset.saturating_sub(1);
        for (index, line) in text.lines().enumerate() {
            let line_no = index + 1;
            if line_no < offset || line_no >= offset + limit {
                continue;
            }
            out.push_str(&format!("{line_no:>6}\t{line}\n"));
            shown_end = line_no;
        }

        if shown_end < offset {
            out.push_str("（该行号之后没有内容）\n");
        } else if offset > 1 || shown_end < total {
            out.push_str(&format!(
                "\n（已显示第 {offset}–{shown_end} 行，全文共 {total} 行；用 offset 继续读）\n"
            ));
        }
        Ok(out)
    }
}

async fn list_files(id: &str, raw: &str, dir: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| Error::Tool(format!("列出 {id}/{raw} 失败: {e}")))?;
    let mut names = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| Error::Tool(format!("列出 {id}/{raw} 失败: {e}")))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let suffix = if entry.path().is_dir() { "/" } else { "" };
        names.push(format!("{name}{suffix}"));
    }
    names.sort();
    if names.is_empty() {
        return Ok(format!("技能 {id}/{raw} 是空目录"));
    }
    Ok(format!(
        "技能 {id}/{raw} 是目录，包含：\n{}",
        names
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("goldenagent-skill-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, id: &str, body: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), body).unwrap();
    }

    #[test]
    fn parses_name_and_description() {
        let meta = parse_meta("---\nname: PDF 处理\ndescription: 处理 PDF 时用\n---\n正文\n");
        assert_eq!(meta.name.as_deref(), Some("PDF 处理"));
        assert_eq!(meta.description.as_deref(), Some("处理 PDF 时用"));
    }

    #[test]
    fn strips_quotes_around_values() {
        let meta = parse_meta("---\nname: \"a: b\"\ndescription: 'x'\n---\n");
        assert_eq!(meta.name.as_deref(), Some("a: b"));
        assert_eq!(meta.description.as_deref(), Some("x"));
    }

    #[test]
    fn ignores_document_without_frontmatter() {
        let meta = parse_meta("# 标题\n没有 frontmatter\n");
        assert!(meta.description.is_none());
    }

    #[test]
    fn requires_the_fence_to_be_alone_on_its_line() {
        // `----` 是分隔线，不是 frontmatter 开头
        let meta = parse_meta("----\ndescription: 不应该被读到\n---\n");
        assert!(meta.description.is_none());
    }

    #[test]
    fn rejects_multiline_description_instead_of_taking_the_marker() {
        let meta = parse_meta("---\ndescription: >\n  折成多行\n---\n");
        assert!(meta.description.is_none(), "不该把 `>` 当成描述");
    }

    #[test]
    fn scans_sorted_and_reports_skipped() {
        let root = temp_dir("scan");
        write_skill(&root, "beta", "---\ndescription: 第二个\n---\n");
        write_skill(&root, "alpha", "---\ndescription: 第一个\n---\n");
        write_skill(&root, "gamma", "没有 frontmatter\n");
        std::fs::create_dir_all(root.join("Bad_Name")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();

        let report = scan(&root);
        let ids: Vec<&str> = report.skills.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "beta"], "必须排序，否则提示词不稳定");

        let skipped: Vec<&str> = report.skipped.iter().map(|s| s.id.as_str()).collect();
        assert!(skipped.contains(&"gamma"), "缺 description 要报出来");
        assert!(skipped.contains(&"Bad_Name"), "id 不合法要报出来");
        assert!(!skipped.contains(&".hidden"), "隐藏目录直接忽略，不算跳过");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_is_empty_not_error() {
        let report = scan(Path::new("Z:\\definitely-not-here"));
        assert!(report.skills.is_empty());
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn description_is_clipped_not_dropped() {
        let long = "很".repeat(MAX_DESCRIPTION_CHARS + 50);
        let text = format!("---\ndescription: {long}\n---\n");
        let meta = parse_meta(&text);
        assert_eq!(meta.description.unwrap().chars().count(), MAX_DESCRIPTION_CHARS + 50);

        let root = temp_dir("clip");
        write_skill(&root, "long", &text);
        let report = scan(&root);
        let skill = &report.skills[0];
        assert_eq!(skill.description.chars().count(), MAX_DESCRIPTION_CHARS + 1);
        assert!(skill.description.ends_with('…'));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn caps_the_number_of_skills() {
        let root = temp_dir("cap");
        for i in 0..(MAX_SKILLS + 3) {
            write_skill(&root, &format!("s{i:02}"), "---\ndescription: x\n---\n");
        }
        let report = scan(&root);
        assert_eq!(report.skills.len(), MAX_SKILLS);
        assert_eq!(report.skipped.len(), 3);
        assert!(report.skipped[0].reason.contains("上限"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn valid_ids_are_whitelisted() {
        assert!(is_valid_id("pdf"));
        assert!(is_valid_id("a1-b2"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("PDF"));
        assert!(!is_valid_id("-lead"));
        assert!(!is_valid_id("../x"));
        assert!(!is_valid_id("a b"));
        assert!(!is_valid_id(&"a".repeat(65)));
    }
}
