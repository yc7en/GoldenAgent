//! B20: 只读调研型子代理（Subagent Delegation）验收测试。
//!
//! 验证契约（A-5 与 A-15）：
//! 1. 工具声明符合规范（ToolKind::Read，包含 role/prompt/summary）；
//! 2. 子代理使用独立瞬时会话，搜索探索过程不进入外部会话，仅回传蒸馏摘要；
//! 3. 子代理严格只读，坚决拒绝写操作且不反向弹窗；
//! 4. 触达 max_turns 时安全截断并附带提示；
//! 5. AgentLoop 默认自动挂载 invoke_subagent 工具。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use goldenagent_core::{ContentBlock, Message, ToolCall, ToolKind};
use goldenagent_loop::subagent::InvokeSubagent;
use goldenagent_loop::{AgentLoop, Config};
use goldenagent_policy::{Mode, Policy};
use goldenagent_provider::MockProvider;
use goldenagent_tools::{Registry, Tool, ToolContext};
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Workspace {
    path: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "goldenagent-subagent-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn text(value: &str) -> Message {
    Message::assistant(vec![ContentBlock::Text {
        text: value.into(),
    }])
}

fn call(id: &str, name: &str, input: serde_json::Value) -> Message {
    Message::assistant(vec![ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        input,
    })])
}

#[test]
fn subagent_spec_conforms_to_schema() {
    let mock = Arc::new(MockProvider::new(vec![]));
    let read_only = Arc::new(Registry::new());
    let subagent = InvokeSubagent::new(mock, read_only, "mock-model".into(), 6);
    let spec = subagent.spec();

    assert_eq!(spec.name, "invoke_subagent");
    assert_eq!(spec.kind, ToolKind::Read);
    let req = spec.input_schema["required"].as_array().unwrap();
    let req_strs: Vec<&str> = req.iter().filter_map(|v| v.as_str()).collect();
    assert!(req_strs.contains(&"role"));
    assert!(req_strs.contains(&"prompt"));
}

#[tokio::test]
async fn subagent_distills_result_and_isolates_intermediate_steps() {
    let ws = Workspace::new();
    std::fs::write(ws.path.join("target.rs"), "pub fn compute() -> i32 { 42 }").unwrap();

    // 模拟子代理内部的调用序列：
    // 轮次 1：调用 read_file 读取 target.rs
    // 轮次 2：分析完毕，输出最终结论
    let provider = Arc::new(MockProvider::new(vec![
        call("c1", "read_file", json!({ "path": "target.rs" })),
        text("经过调研，compute 函数定义在 target.rs 第 1 行，返回固定值 42。"),
    ]));

    let defaults = Registry::with_defaults();
    let read_only = Arc::new(defaults.read_only());
    let subagent = InvokeSubagent::new(provider, read_only, "mock-model".into(), 6);

    let ctx = ToolContext::new(&ws.path);
    let input = json!({
        "role": "code_researcher",
        "prompt": "排查 compute 函数的实现与返回值",
        "summary": "调研核心计算逻辑"
    });

    let result = subagent.run(input, &ctx).await.expect("子代理应该成功执行");

    assert!(
        result.contains("compute 函数定义在 target.rs"),
        "必须返回提炼后的结论：{result}"
    );
    assert!(result.contains("42"), "必须包含关键分析数据：{result}");
}

#[tokio::test]
async fn subagent_denies_write_operations() {
    let ws = Workspace::new();

    // 模拟恶意或失控的模型尝试在子代理内部调用 write_file
    let provider = Arc::new(MockProvider::new(vec![
        call(
            "c1",
            "write_file",
            json!({ "path": "danger.txt", "content": "malicious" }),
        ),
        text("写文件失败，那我直接汇报只读排查结论。"),
    ]));

    // 子代理的工具集只挂载只读工具
    let defaults = Registry::with_defaults();
    let read_only = Arc::new(defaults.read_only().without("invoke_subagent"));
    let subagent = InvokeSubagent::new(provider, read_only, "mock-model".into(), 6);

    let ctx = ToolContext::new(&ws.path);
    let input = json!({
        "role": "code_researcher",
        "prompt": "尝试写文件"
    });

    let result = subagent.run(input, &ctx).await.expect("执行不应崩溃");
    assert!(!ws.path.join("danger.txt").exists(), "子代理绝对不能创建文件");
    assert!(
        result.contains("那我直接汇报只读排查结论"),
        "模型能根据拒绝结果继续处理：{result}"
    );
}

#[tokio::test]
async fn subagent_caps_at_max_turns() {
    let ws = Workspace::new();
    std::fs::write(ws.path.join("a.txt"), "hello").unwrap();

    // 循环无限请求 read_file
    let provider = Arc::new(MockProvider::new(vec![
        call("c1", "read_file", json!({ "path": "a.txt" })),
        call("c2", "read_file", json!({ "path": "a.txt" })),
        call("c3", "read_file", json!({ "path": "a.txt" })),
    ]));

    let defaults = Registry::with_defaults();
    let read_only = Arc::new(defaults.read_only());
    // 设置最大轮次为 2
    let subagent = InvokeSubagent::new(provider, read_only, "mock-model".into(), 2);

    let ctx = ToolContext::new(&ws.path);
    let input = json!({
        "role": "code_researcher",
        "prompt": "反复读取 a.txt"
    });

    let result = subagent.run(input, &ctx).await.expect("到上限不应报错");
    assert!(
        result.contains("已达到最大轮次上限 (2 轮)"),
        "必须明确指出达到上限：{result}"
    );
}

#[test]
fn agent_loop_automatically_registers_subagent_tool() {
    let ws = Workspace::new();
    let provider = Arc::new(MockProvider::new(vec![]));
    let tools = Arc::new(Registry::with_defaults());
    let policy = Policy::new(Mode::ReadOnly, &ws.path);
    let agent = AgentLoop::new(provider, tools, policy, Config::default());

    assert!(
        agent.tools().contains("invoke_subagent"),
        "默认配置的 AgentLoop 必须包含 invoke_subagent 工具"
    );
}

#[tokio::test]
async fn subagent_background_spawn_returns_task_id() {
    let ws = Workspace::new();
    std::fs::write(ws.path.join("a.txt"), "hello").unwrap();

    let provider = Arc::new(MockProvider::new(vec![
        call("c1", "read_file", json!({ "path": "a.txt" })),
        text("后台子代理分析完成：a.txt 内容正常。"),
    ]));

    let defaults = Registry::with_defaults();
    let read_only = Arc::new(defaults.read_only());
    let subagent = InvokeSubagent::new(provider, read_only, "mock-model".into(), 4);

    let ctx = ToolContext::new(&ws.path);
    let input = json!({
        "role": "async_researcher",
        "prompt": "后台排查 a.txt",
        "background": true
    });

    let result = subagent.run(input, &ctx).await.expect("派发后台必须成功");
    assert!(result.contains("后台子代理调研已启动 (TaskId: task_"));

    // 等待后台子代理协程跑完
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let list = ctx.tasks.list();
    assert!(!list.is_empty());
    let task = list.last().unwrap();
    assert_eq!(task.status, goldenagent_core::TaskStatus::Completed);
    assert!(task.output.as_ref().unwrap().contains("后台子代理分析完成"));
}
