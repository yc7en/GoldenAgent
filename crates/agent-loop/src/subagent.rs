use std::sync::Arc;

use async_trait::async_trait;
use goldenagent_core::{Result, Role, ToolKind, ToolSpec};
use goldenagent_policy::{Mode, Policy};
use goldenagent_provider::Provider;
use goldenagent_session::Session;
use goldenagent_tools::{required_str, Registry, Tool, ToolContext};
use serde_json::json;

use crate::approver::AutoDeny;
use crate::cancel::Cancellation;
use crate::{AgentLoop, Config, EventSink, Outcome};

pub const DEFAULT_SUBAGENT_MAX_TURNS: u32 = 6;

const SUBAGENT_SYSTEM_PROMPT: &str = "你是 GoldenAgent 的只读调研子代理（Subagent）。\
你的职责是：根据分配的角色定位与任务目标，在工作区内深入排查、阅读文件、搜索定位代码。\
硬约束：\
1. 你只有只读权限（read_file、list_dir、grep、glob、use_skill），严禁修改文件或执行系统命令；\
2. 当你掌握了足够的事实、得出明确结论后，请直接输出高度提炼、结构清晰的中文总结，汇报给主 Agent；\
3. 结论必须具体、有依有据（明确指出关键文件路径和行号/关键逻辑），但不要倾倒大段未处理的原始文件内容。";

/// B20 & B21：只读调研型子代理工具（支持同步等待与后台异步派发）。
///
/// 遵循 ARCHITECTURE A-5, A-15 与 A-16：
/// - 复用同一个 AgentLoop 架构，非第二套引擎；
/// - 内部使用独立的瞬时会话，繁琐搜索日志不进主会话；
/// - 仅将最终 Assistant 回答文本蒸馏回传给主 Agent；
/// - 支持 background: true 后台独立运行，不阻塞主会话交互。
#[derive(Clone)]
pub struct InvokeSubagent {
    provider: Arc<dyn Provider>,
    read_only_tools: Arc<Registry>,
    model: String,
    max_turns: u32,
}

impl InvokeSubagent {
    pub fn new(
        provider: Arc<dyn Provider>,
        read_only_tools: Arc<Registry>,
        model: String,
        max_turns: u32,
    ) -> Self {
        Self {
            provider,
            read_only_tools,
            model,
            max_turns: if max_turns == 0 {
                DEFAULT_SUBAGENT_MAX_TURNS
            } else {
                max_turns
            },
        }
    }
}

#[async_trait]
impl Tool for InvokeSubagent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "invoke_subagent".into(),
            kind: ToolKind::Read,
            description: "委派一个只读子代理（Subagent）去执行深入的代码搜索、文件分析与调研。子代理拥有独立的上下文和只读工具集（read_file, list_dir, grep, glob, use_skill），繁杂的搜索过程不会污染主会话上下文，并在完成后返回高度提炼的结论摘要。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "子代理角色定位，如 'code_researcher'、'dependency_checker'、'error_tracer'"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "清晰明确的子任务调研目标与具体要求"
                    },
                    "summary": {
                        "type": "string",
                        "description": "4-8 字中文操作意图，如 '调研渲染管线'、'排查依赖版本'"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "是否在后台独立运行，默认 false。设为 true 时立即返回 task_id，不阻塞主会话交互"
                    }
                },
                "required": ["role", "prompt"]
            }),
        }
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let role = required_str(&input, "role")?;
        let background = input
            .get("background")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if background {
            let this = self.clone();
            let input_clone = input.clone();
            let ctx_clone = ctx.clone();
            let role_desc = format!("子代理调研: {role}");
            let task_id = ctx.tasks.spawn("subagent", &role_desc, async move {
                this.run_sync(input_clone, &ctx_clone).await
            });
            return Ok(format!(
                "后台子代理调研已启动 (TaskId: {task_id})。\n角色: {role}\n任务正在后台独立运行，不会阻塞主会话。你可以使用 manage_task 查询进度或取消。"
            ));
        }

        self.run_sync(input, ctx).await
    }
}

impl InvokeSubagent {
    async fn run_sync(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let role = required_str(&input, "role")?;
        let prompt = required_str(&input, "prompt")?;

        // A-15: 子代理严格只读，继承当前工作区所有根目录
        let sub_policy = Policy::with_roots(Mode::ReadOnly, ctx.roots());

        let sub_config = Config {
            model: self.model.clone(),
            system: format!(
                "{SUBAGENT_SYSTEM_PROMPT}\n\n当前角色：{role}\n工作区：{}",
                ctx.workspace.display()
            ),
            max_tokens: 4096,
            max_turns: self.max_turns,
            compact_at: None,
            compact_keep: 4,
            compact_keep_tokens: None,
            compact_summary: false,
            summary_max_tokens: 1024,
            skills_root: ctx.skills_root.clone(),
            enable_subagent: false, // 严格禁止递归创建子代理
            subagent_max_turns: 0,
            tasks: Some(Arc::clone(&ctx.tasks)),
        };

        let sub_agent = AgentLoop::new(
            Arc::clone(&self.provider),
            Arc::clone(&self.read_only_tools),
            sub_policy,
            sub_config,
        );

        let mut sub_session = Session::new();
        let (sub_sink, mut sub_rx) = EventSink::channel();

        // 消费子代理产生的中间事件，保持内部事件通道畅通
        tokio::spawn(async move {
            while let Some(_event) = sub_rx.recv().await {}
        });

        // 自动拒绝任何非只读授权请求（防止交互弹窗死锁）
        let sub_approver = AutoDeny;
        let sub_cancel = Cancellation::new();

        let outcome = sub_agent
            .run(&mut sub_session, prompt, &sub_sink, &sub_approver, &sub_cancel)
            .await?;

        // 信息蒸馏（Distillation）：提取最后一轮有效 assistant 回答作为结果回传给主 Agent
        let distilled = sub_session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.text())
            .filter(|t| !t.trim().is_empty());

        match outcome {
            Outcome::Completed { .. } => match distilled {
                Some(text) => Ok(text),
                None => Ok("子代理调研完成，但未生成文字结论。".to_string()),
            },
            Outcome::MaxTurns => {
                let tail = distilled.unwrap_or_else(|| "（未生成结论）".to_string());
                Ok(format!(
                    "[子代理已达到最大轮次上限 ({} 轮)，提前截断]\n{}",
                    self.max_turns, tail
                ))
            }
            Outcome::Cancelled => Ok("子代理任务已取消。".to_string()),
        }
    }
}
