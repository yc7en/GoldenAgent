use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use goldenagent_core::{Error, Result, SpillHandle, ToolKind, ToolSpec};
use serde_json::json;

use crate::jail;
use crate::{Tool, ToolContext};

pub const DEFAULT_SPILL_THRESHOLD_BYTES: usize = 32 * 1024;
pub const SPILL_DIR_NAME: &str = ".goldenagent/spill";
pub const PREVIEW_LINES: usize = 50;

static SPILL_SEQ: AtomicU32 = AtomicU32::new(0);

fn rand_salt() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    nanos ^ SPILL_SEQ.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9)
}

/// 溢出存储管理器。
///
/// 遵循 ARCHITECTURE.md A-20：
/// 拦截工具超配额输出（默认 32 KiB），落盘保存完整内容，
/// 构造包含首尾预览和元数据的结构化句柄。
#[derive(Debug, Clone)]
pub struct SpillManager {
    root: PathBuf,
    threshold_bytes: usize,
}

impl SpillManager {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let ws = workspace.into();
        Self {
            root: ws.join(SPILL_DIR_NAME),
            threshold_bytes: DEFAULT_SPILL_THRESHOLD_BYTES,
        }
    }

    pub fn with_root(root: impl Into<PathBuf>, threshold_bytes: usize) -> Self {
        Self {
            root: root.into(),
            threshold_bytes,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn threshold_bytes(&self) -> usize {
        self.threshold_bytes
    }

    /// 检查输出是否超过阈值；若超过则持久化并返回结构化句柄，否则返回 Ok(None)。
    pub async fn maybe_spill(&self, output: &str) -> Result<Option<SpillHandle>> {
        if output.len() <= self.threshold_bytes {
            return Ok(None);
        }

        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|e| Error::Tool(format!("创建 spill 目录失败: {e}")))?;

        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        let salt = rand_salt();
        let file_name = format!("spill_{ms}_{salt:04x}.log");
        let file_path = self.root.join(&file_name);

        tokio::fs::write(&file_path, output)
            .await
            .map_err(|e| Error::Tool(format!("写入 spill 日志失败: {e}")))?;

        let lines: Vec<&str> = output.lines().collect();
        let total_lines = lines.len();
        let total_bytes = output.len();

        let preview_head = lines
            .iter()
            .take(PREVIEW_LINES)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");

        let preview_tail = if total_lines > PREVIEW_LINES {
            let skip = total_lines.saturating_sub(PREVIEW_LINES);
            lines
                .iter()
                .skip(skip)
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };

        let relative_file = format!("{SPILL_DIR_NAME}/{file_name}");
        Ok(Some(SpillHandle::new(
            relative_file,
            total_lines,
            total_bytes,
            preview_head,
            preview_tail,
        )))
    }
}

/// 定向查阅溢出日志工具（InspectSpill）。
///
/// 供模型在拿到 SpillHandle 后，根据预览尾部报错等信息按需翻阅特定行范围，
/// 避免全量加载打爆上下文。
pub struct InspectSpill;

#[async_trait]
impl Tool for InspectSpill {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "inspect_spill",
            "读取因输出过大而溢出到磁盘的日志文件特定行范围（行号从 1 开始计数）",
            json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "spill 日志文件路径或文件名（如 .goldenagent/spill/spill_xxx.log 或 spill_xxx.log）"
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "起始行号（从 1 开始，默认为 1）",
                        "minimum": 1
                    },
                    "count": {
                        "type": "integer",
                        "description": "读取行数（默认为 100，最大 1000）",
                        "minimum": 1,
                        "maximum": 1000
                    },
                    "summary": {
                        "type": "string",
                        "description": "查阅意图简要说明（4-8字，如：查看编译器末尾报错）"
                    }
                },
                "required": ["file"]
            }),
            ToolKind::Read,
        )
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let raw_file = input
            .get("file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Tool("缺少参数 'file'".into()))?;

        let start_line = input
            .get("start_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;

        let count = input
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .clamp(1, 1000) as usize;

        // 安全路径解析：支持 .goldenagent/spill/xxx 或直接文件名，并在 spill_root 下做 jail 防越界校验
        let spill_root = ctx.spill.root();
        let stripped_file = raw_file
            .trim_start_matches('/')
            .trim_start_matches('\\')
            .strip_prefix(".goldenagent/spill/")
            .or_else(|| raw_file.strip_prefix(".goldenagent\\spill\\"))
            .unwrap_or(raw_file);

        let target_path = jail::resolve(spill_root, stripped_file)
            .map_err(|e| Error::Tool(format!("无效或越界的 spill 路径 ({raw_file}): {e}")))?;

        let content = tokio::fs::read_to_string(&target_path)
            .await
            .map_err(|e| Error::Tool(format!("读取 spill 文件 {} 失败: {e}", target_path.display())))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        if start_line > total {
            return Ok(format!(
                "请求起始行 {} 超出文件总行数（总行数: {}）",
                start_line, total
            ));
        }

        let slice_start = start_line - 1;
        let slice_end = (slice_start + count).min(total);
        let selected = &lines[slice_start..slice_end];

        let mut output = String::new();
        output.push_str(&format!(
            "--- [{}] 第 {}–{} 行 (共 {} 行) ---\n",
            raw_file,
            start_line,
            slice_end,
            total
        ));

        for (idx, line) in selected.iter().enumerate() {
            let current_line_num = start_line + idx;
            output.push_str(&format!("{:5} | {}\n", current_line_num, line));
        }

        Ok(output)
    }
}
