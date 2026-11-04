//! 第 3 层：唯一的 agent 循环（A-5）。
//!
//! 一次 `run()` 就是：写用户消息 → 反复「问模型 → 执行工具 → 回灌结果」→ 直到没有工具调用。
//!
//! 三条硬约束：
//!
//! 1. **只发 [`Event`]，不返回内部结构**。调用方想知道进度，只能消费事件；
//! 2. **不做权限判断**。该不该做问 [`goldenagent_policy`]（A-4），
//!    需要人工点头问 [`Approver`]（A-7），路径越界由 `tools::jail` 硬拒绝（A-6）；
//! 3. **不持有隐藏状态**。会话从外部传进来、改完就地生效，进程重启后接着跑。

pub mod approver;
pub mod cancel;

pub use approver::{ApprovalRequest, Approver, AutoAllow, AutoDeny};
pub use cancel::Cancellation;

use std::sync::Arc;

use goldenagent_core::{Error, Event, Message, Result, StopReason, ToolCall, ToolResult};
use goldenagent_policy::{Decision, Policy};
use goldenagent_provider::{Completion, CompletionRequest, Provider, StreamChunk};
use goldenagent_session::Session;
use goldenagent_tools::{Registry, ToolContext};
use tokio::sync::mpsc;

/// 单条工具结果推给前端时的最大字符数。
///
/// 模型拿到的是完整结果（它要用它干活），事件里只放预览——
/// 否则一个 1 MiB 的 `read_file` 会把前端和 IPC 一起拖死。
pub const EVENT_OUTPUT_LIMIT: usize = 8 * 1024;

/// 事件出口。包一层是为了让"接收端没了"这件事只在一个地方判断。
pub struct EventSink {
    tx: mpsc::UnboundedSender<Event>,
}

impl EventSink {
    pub fn new(tx: mpsc::UnboundedSender<Event>) -> Self {
        Self { tx }
    }

    /// 建一对收发端。测试和 Tauri 壳都用这个。
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// 发事件。接收端已关闭时静默丢弃——这是"用户关了窗口"，不是错误。
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// 一次 `run()` 的结果。只说明"为什么结束"，过程全在事件里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 模型没有更多工具调用，正常结束。
    Completed { turns: u32 },
    /// 达到 `max_turns` 上限。
    MaxTurns,
    /// 被取消或事件接收端关闭。
    Cancelled,
}

/// 一轮模型调用的结局。
///
/// 取消单独算一类：它不是错误（不发 `Event::Error`），也不该被当成一次正常回答。
enum TurnEnd {
    Done(Completion),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub system: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    /// token 上限阈值。`None` 表示关闭上下文压缩。
    pub compact_at: Option<usize>,
    /// 压缩后保留的消息条数。
    pub compact_keep: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "mock".into(),
            system: String::new(),
            max_tokens: 4096,
            max_turns: 12,
            compact_at: None,
            compact_keep: 8,
        }
    }
}

impl Config {
    /// 打开上下文压缩。
    pub fn with_compaction(mut self, at: usize, keep: usize) -> Self {
        self.compact_at = Some(at);
        self.compact_keep = keep;
        self
    }
}

pub struct AgentLoop {
    provider: Arc<dyn Provider>,
    tools: Arc<Registry>,
    policy: Policy,
    ctx: ToolContext,
    config: Config,
}

impl AgentLoop {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<Registry>,
        policy: Policy,
        config: Config,
    ) -> Self {
        // 工作区只有 policy 一个来源，避免两处配置漂移。
        //
        // 工具拿到的根集合必须与 policy 判定用的完全一致：两处各取一份，
        // 就会出现"policy 放行、jail 拒绝"（或反过来）的分歧。
        let roots = policy.roots();
        let ctx = match roots.split_first() {
            Some((primary, rest)) => ToolContext::with_roots(primary.clone(), rest.to_vec()),
            None => ToolContext::new(policy.workspace()),
        };
        Self {
            provider,
            tools,
            policy,
            ctx,
            config,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// 跑一轮完整对话。`session` 就地更新，可直接落盘续跑。
    pub async fn run(
        &self,
        session: &mut Session,
        prompt: &str,
        events: &EventSink,
        approver: &dyn Approver,
        cancel: &Cancellation,
    ) -> Result<Outcome> {
        session.push_user(prompt);

        for turn in 1..=self.config.max_turns {
            if cancel.is_cancelled() || events.is_closed() {
                events.send(Event::Finished {
                    reason: StopReason::Cancelled,
                });
                return Ok(Outcome::Cancelled);
            }

            self.maybe_compact(session, events);

            events.send(Event::TurnStarted { turn });

            let request = CompletionRequest {
                model: self.config.model.clone(),
                system: self.config.system.clone(),
                messages: session.messages().to_vec(),
                tools: self.tools.specs(),
                max_tokens: self.config.max_tokens,
            };

            let completion = match self.stream_turn(request, events, cancel).await {
                Ok(TurnEnd::Done(completion)) => completion,
                Ok(TurnEnd::Cancelled) => {
                    events.send(Event::Finished {
                        reason: StopReason::Cancelled,
                    });
                    return Ok(Outcome::Cancelled);
                }
                Err(error) => {
                    events.send(Event::Error {
                        message: error.to_string(),
                    });
                    events.send(Event::Finished {
                        reason: StopReason::Error,
                    });
                    return Err(error);
                }
            };

            // 文本已经在 stream_turn 里逐片发过了，这里**不再补发**——
            // 补发会让前端把同一段话显示两遍。
            session.push(completion.message.clone());

            let calls = completion.message.tool_calls();
            if calls.is_empty() {
                events.send(Event::TurnFinished { turn });
                events.send(Event::Finished {
                    reason: final_reason(completion.stop_reason),
                });
                return Ok(Outcome::Completed { turns: turn });
            }

            if cancel.is_cancelled() {
                events.send(Event::Finished {
                    reason: StopReason::Cancelled,
                });
                return Ok(Outcome::Cancelled);
            }

            let mut results = Vec::with_capacity(calls.len());
            for call in calls {
                results.push(self.handle_call(call, events, approver, cancel).await);
                if cancel.is_cancelled() {
                    break;
                }
            }

            // 工具结果以 user 消息回灌，这是唯一的"喂回去"通道。
            session.push(Message::tool_results(results));
            events.send(Event::TurnFinished { turn });
        }

        events.send(Event::Finished {
            reason: StopReason::MaxTurns,
        });
        Ok(Outcome::MaxTurns)
    }

    /// 一轮模型调用：流式收片段，收到一片就发一片。
    ///
    /// 放进 `tokio::spawn` 是被迫的：写通道和读通道必须同时进行，
    /// 否则通道缓冲一满（64 条），provider 就卡死在 `send` 上，
    /// 而循环正等着它返回——双方互等。
    ///
    /// 取消也在这里生效：模型可能要说几十秒，用户按了停止不该等它说完。
    async fn stream_turn(
        &self,
        request: CompletionRequest,
        events: &EventSink,
        cancel: &Cancellation,
    ) -> Result<TurnEnd> {
        let (tx, mut rx) = mpsc::channel::<StreamChunk>(64);
        let provider = Arc::clone(&self.provider);
        let task = tokio::spawn(async move { provider.stream(request, tx).await });

        loop {
            tokio::select! {
                chunk = rx.recv() => match chunk {
                    Some(StreamChunk::TextDelta(text)) => {
                        events.send(Event::TextDelta { text });
                        // 窗口关了就该收手，别对着一个没人收的通道继续烧 token。
                        if events.is_closed() {
                            task.abort();
                            return Ok(TurnEnd::Cancelled);
                        }
                    }
                    // 思考照原样转出去，但**不进入 Completion**——
                    // 那一层只放要写进历史的东西。
                    Some(StreamChunk::ThinkingDelta(text)) => {
                        events.send(Event::ThinkingDelta { text });
                        if events.is_closed() {
                            task.abort();
                            return Ok(TurnEnd::Cancelled);
                        }
                    }
                    // 通道关闭 = provider 那边已经结束（或把发送端丢了）。
                    None => break,
                },
                _ = cancel.cancelled() => {
                    task.abort();
                    return Ok(TurnEnd::Cancelled);
                }
            }
        }

        match task.await {
            Ok(result) => result.map(TurnEnd::Done),
            // abort 之后走到这里，不算错误。
            Err(error) if error.is_cancelled() => Ok(TurnEnd::Cancelled),
            Err(error) => Err(Error::Provider(format!("provider 任务异常结束：{error}"))),
        }
    }

    /// 一次工具调用的完整生命周期：裁决 → 授权 → 执行 → 发事件 → 返回结果给模型。
    ///
    /// **不管放行还是拒绝，都会成对发出 `ToolStarted` / `ToolFinished`。**
    /// `ToolStarted` 的语义是"开始处理这个调用"，不是"已经执行"——
    /// 这样前端可以只靠这对事件维护工具卡片的生命周期，不必再判一次状态。
    async fn handle_call(
        &self,
        call: ToolCall,
        events: &EventSink,
        approver: &dyn Approver,
        cancel: &Cancellation,
    ) -> ToolResult {
        let spec = self.tools.spec(&call.name);

        // 未知工具：模型编了个不存在的名字。也要可观测，不能静默。
        let Some(spec) = spec else {
            let output = format!("未知工具: {}", call.name);
            self.emit_started(events, &call);
            self.emit_finished(events, &call.id, &call.name, false, &output);
            return ToolResult {
                id: call.id,
                ok: false,
                output,
            };
        };

        match self.policy.decide(&spec, &call.input) {
            Decision::Allow => {}
            Decision::Deny { reason } => {
                let output = format!("已拒绝: {reason}");
                self.emit_started(events, &call);
                self.emit_finished(events, &call.id, &call.name, false, &output);
                return ToolResult {
                    id: call.id,
                    ok: false,
                    output,
                };
            }
            Decision::Ask { summary } => {
                events.send(Event::PermissionRequested {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    summary: summary.clone(),
                });

                let approved = approver
                    .approve(ApprovalRequest {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        summary,
                    })
                    .await;

                if !approved {
                    let output = "未获得授权，已跳过".to_string();
                    self.emit_started(events, &call);
                    self.emit_finished(events, &call.id, &call.name, false, &output);
                    return ToolResult {
                        id: call.id,
                        ok: false,
                        output,
                    };
                }
            }
        }

        if cancel.is_cancelled() {
            let output = "已取消".to_string();
            self.emit_started(events, &call);
            self.emit_finished(events, &call.id, &call.name, false, &output);
            return ToolResult {
                id: call.id,
                ok: false,
                output,
            };
        }

        self.emit_started(events, &call);
        let (ok, output) = match self.tools.run(&call.name, call.input.clone(), &self.ctx).await {
            Ok(output) => (true, output),
            Err(error) => (false, error.to_string()),
        };
        self.emit_finished(events, &call.id, &call.name, ok, &output);

        ToolResult {
            id: call.id,
            ok,
            output,
        }
    }

    fn emit_started(&self, events: &EventSink, call: &ToolCall) {
        events.send(Event::ToolStarted {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        });
    }

    fn emit_finished(&self, events: &EventSink, id: &str, name: &str, ok: bool, output: &str) {
        events.send(Event::ToolFinished {
            id: id.to_string(),
            name: name.to_string(),
            ok,
            output: preview(output),
        });
    }

    /// 压缩历史。任何丢弃历史的行为都必须发事件（ARCHITECTURE §4）。
    fn maybe_compact(&self, session: &mut Session, events: &EventSink) {
        let Some(limit) = self.config.compact_at else {
            return;
        };
        if session.estimate_tokens() <= limit {
            return;
        }
        let removed = session.compact(self.config.compact_keep);
        if removed > 0 {
            events.send(Event::ContextCompacted { removed });
        }
    }
}

/// 截断成事件预览。截断点放在字符边界上，不会把 UTF-8 切坏。
fn preview(output: &str) -> String {
    if output.chars().count() <= EVENT_OUTPUT_LIMIT {
        return output.to_string();
    }
    let head: String = output.chars().take(EVENT_OUTPUT_LIMIT).collect();
    let omitted = output.chars().count() - EVENT_OUTPUT_LIMIT;
    format!("{head}…（事件预览已截断 {omitted} 字符，模型收到的是完整内容）")
}

/// 只把"真的因为长度停下"透出去，其余一律按正常结束处理。
fn final_reason(reason: StopReason) -> StopReason {
    match reason {
        StopReason::MaxTokens => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}
