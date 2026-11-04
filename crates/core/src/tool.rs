use serde::{Deserialize, Serialize};

/// 工具读写属性。policy 层靠它做判断，避免硬编码工具名。
///
/// 三档而不是两档，是因为**执行类工具没法用"路径在不在工作区内"来判断**：
/// 一条 `rm -rf ~` 的 cwd 就在工作区里，但它干的事和工作区没关系。
/// 把它硬塞进 `Write` 会让 policy 得出"工作区可写档就放行"的错误结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Write,
    /// 执行任意命令。policy 对它只能"问"或"拒"，不能按路径放行。
    Exec,
}

/// 给模型看的工具声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub kind: ToolKind,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        kind: ToolKind,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            kind,
        }
    }
}
