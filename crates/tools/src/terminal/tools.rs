use crate::{jail, required_str, Tool, ToolContext};
use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

/// 创建有状态终端会话
pub struct TerminalCreate;

#[async_trait]
impl Tool for TerminalCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal_create".into(),
            description: "创建并启动一个长期存活的有状态终端会话（PTY），状态（如 cd、环境变量）在后续调用中持久保留。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "终端会话自定义名称（如 'dev-server'）"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "起始工作目录（相对路径挂工作区主根），缺省为主工作区根目录"
                    },
                    "shell": {
                        "type": "string",
                        "description": "指定 Shell 命令（缺省为系统默认 cmd/sh）"
                    },
                    "summary": {
                        "type": "string",
                        "description": "4-8字操作意图"
                    }
                }
            }),
            kind: ToolKind::Exec,
        }
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let shell = input.get("shell").and_then(|v| v.as_str());

        let cwd = match input.get("cwd").and_then(|v| v.as_str()) {
            Some(raw) => jail::resolve_in(&ctx.roots(), raw)?,
            None => ctx.workspace.clone(),
        };

        let session = ctx
            .terminals
            .create_session(name, cwd.clone(), shell)
            .map_err(Error::Tool)?;

        // 稍微等待 300ms 捕获终端初始 banner / prompt
        let (initial_output, is_alive, _) = session.read_output(500, 200).await;

        Ok(json!({
            "terminal_id": session.id,
            "name": session.name,
            "cwd": cwd.to_string_lossy(),
            "initial_output": initial_output.trim(),
            "is_alive": is_alive,
        })
        .to_string())
    }
}

/// 向有状态终端写入命令或输入字符
pub struct TerminalWrite;

#[async_trait]
impl Tool for TerminalWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal_write".into(),
            description: "向指定的有状态终端会话写入命令、交互确认（如 'y\\n'）或控制字符（如 '\\x03' 中断）。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "terminal_id": {
                        "type": "string",
                        "description": "终端会话 ID"
                    },
                    "input": {
                        "type": "string",
                        "description": "要写入的命令字符串或输入文本（执行命令需包含 '\\n'）"
                    },
                    "summary": {
                        "type": "string",
                        "description": "4-8字操作意图"
                    }
                },
                "required": ["terminal_id", "input"]
            }),
            kind: ToolKind::Exec,
        }
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let terminal_id = required_str(&input, "terminal_id")?;
        let text = required_str(&input, "input")?;

        let session = ctx
            .terminals
            .get(terminal_id)
            .ok_or_else(|| Error::Tool(format!("未找到终端会话: {terminal_id}")))?;

        let bytes_written = session.write_input(text).map_err(Error::Tool)?;

        Ok(json!({
            "terminal_id": terminal_id,
            "bytes_written": bytes_written,
        })
        .to_string())
    }
}

/// 读取有状态终端屏幕增量输出
pub struct TerminalRead;

#[async_trait]
impl Tool for TerminalRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal_read".into(),
            description: "从指定的有状态终端读取最新输出增量。支持静默检测（Inactivity Detection），在输出暂停或等待用户交互时即刻返回。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "terminal_id": {
                        "type": "string",
                        "description": "终端会话 ID"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "最大等待超时毫秒数（缺省 3000ms）"
                    },
                    "inactivity_ms": {
                        "type": "integer",
                        "description": "输出静默判定毫秒数（缺省 400ms）"
                    },
                    "summary": {
                        "type": "string",
                        "description": "4-8字操作意图"
                    }
                },
                "required": ["terminal_id"]
            }),
            kind: ToolKind::Read,
        }
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let terminal_id = required_str(&input, "terminal_id")?;
        let timeout_ms = input.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(3000);
        let inactivity_ms = input.get("inactivity_ms").and_then(|v| v.as_u64()).unwrap_or(400);

        let session = ctx
            .terminals
            .get(terminal_id)
            .ok_or_else(|| Error::Tool(format!("未找到终端会话: {terminal_id}")))?;

        let (output, is_alive, is_waiting_input) =
            session.read_output(timeout_ms, inactivity_ms).await;

        Ok(json!({
            "terminal_id": terminal_id,
            "output": output,
            "is_alive": is_alive,
            "is_waiting_input": is_waiting_input,
        })
        .to_string())
    }
}

/// 终止并销毁有状态终端
pub struct TerminalKill;

#[async_trait]
impl Tool for TerminalKill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal_kill".into(),
            description: "终止并销毁指定的有状态终端会话进程及其子进程树。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "terminal_id": {
                        "type": "string",
                        "description": "终端会话 ID"
                    },
                    "summary": {
                        "type": "string",
                        "description": "4-8字操作意图"
                    }
                },
                "required": ["terminal_id"]
            }),
            kind: ToolKind::Exec,
        }
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let terminal_id = required_str(&input, "terminal_id")?;
        let killed = ctx.terminals.kill(terminal_id).map_err(Error::Tool)?;

        Ok(json!({
            "terminal_id": terminal_id,
            "killed": killed,
        })
        .to_string())
    }
}
