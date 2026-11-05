//! 第 1 层：工具实现层。
//!
//! 每个工具是一个独立的 `impl Tool`，注册进 [`Registry`]。
//! 加工具不需要改循环、不需要改 policy —— 只要实现 trait 并 register。
//!
//! 这里只负责"能不能做到"，"该不该做"归 policy crate 管。

// jail 对外可见：policy 层要复用同一套路径归一化逻辑，
// 不允许各层各写一份"算不算在工作区内"的判定。
pub mod jail;

mod bash;
mod encoding;
mod glob;
mod grep;
mod list_dir;
mod read_file;
pub mod skill;
mod walk;
mod write_file;
mod apply_patch;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolSpec};

pub use apply_patch::MAX_EDITS;
pub use bash::{DEFAULT_TIMEOUT_SECS, MAX_OUTPUT_BYTES, MAX_TIMEOUT_SECS};
pub use list_dir::MAX_ENTRIES;
pub use read_file::{DEFAULT_LIMIT, MAX_READ_BYTES};
pub use write_file::MAX_WRITE_BYTES;

#[derive(Debug, Clone)]
pub struct ToolContext {
    /// 主目录。`bash` 的默认 cwd、相对路径的落点、`grep`/`glob` 的默认遍历起点。
    pub workspace: PathBuf,
    /// 空间附加的目录：可以读写，但**不**参与相对路径解析。
    pub extra_roots: Vec<PathBuf>,
    /// 技能根目录（只读资产）。
    ///
    /// 与 `workspace` 是**两个概念**：工作区是可读写的现场，技能根是只读的说明书。
    /// 刻意不并进 `roots()`——那样 `write_file` 就写得进 skill 了（见 A-14）。
    pub skills_root: Option<PathBuf>,
}

impl ToolContext {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            extra_roots: Vec::new(),
            skills_root: None,
        }
    }

    /// 多根构造：`primary` 在前，附加目录在后。顺序就是优先级。
    pub fn with_roots(primary: impl Into<PathBuf>, extra: Vec<PathBuf>) -> Self {
        Self {
            workspace: primary.into(),
            extra_roots: extra,
            skills_root: None,
        }
    }

    /// 挂上技能根。只有 `use_skill` 会用它，路径判定仍是单根 + 白名单。
    pub fn with_skills_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.skills_root = Some(root.into());
        self
    }

    /// 主根 + 附加根，供 jail 判定。
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(1 + self.extra_roots.len());
        roots.push(self.workspace.clone());
        roots.extend(self.extra_roots.iter().cloned());
        roots
    }

    /// 工具里解析路径的唯一入口。别自己拼 `workspace.join(...)`——
    /// 那样附加目录永远不会生效，而且会绕过越界检查。
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        jail::resolve_in(&self.roots(), raw)
    }
}

/// 取必填的字符串参数。少参数是最常见的模型犯错，报错要写清缺哪个字段。
pub(crate) fn required_str<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::Tool(format!("缺少参数 {key}（应为非空字符串）")))
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<String>;
}

#[derive(Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 内置工具集。
    ///
    /// 顺序只有展示意义；policy 按 `kind` 判定，顺序不影响权限。
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(read_file::ReadFile));
        // 技能是"让模型自己去读的说明书"，所以它紧跟在 read_file 后面：
        // 两者是同一个动作的两种入口，区别只在能不能越出 skill 目录。
        registry.register(Arc::new(skill::UseSkill));
        registry.register(Arc::new(write_file::WriteFile));
        registry.register(Arc::new(list_dir::ListDir));
        registry.register(Arc::new(grep::Grep));
        registry.register(Arc::new(glob::Glob));
        registry.register(Arc::new(apply_patch::ApplyPatch));
        registry.register(Arc::new(bash::Bash));
        registry
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.push(tool);
        self
    }

    /// 给模型看的工具声明。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn spec(&self, name: &str) -> Option<ToolSpec> {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.spec())
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    pub async fn run(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.spec().name == name)
            .ok_or_else(|| Error::Tool(format!("未知工具: {name}")))?;
        tool.run(input, ctx).await
    }
}
