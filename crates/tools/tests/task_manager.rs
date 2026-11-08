use std::sync::Arc;
use std::time::Duration;

use goldenagent_core::TaskStatus;
use goldenagent_tools::{Registry, TaskManager, ToolContext};
use serde_json::json;

#[tokio::test]
async fn task_manager_spawn_and_complete() {
    let tasks = Arc::new(TaskManager::new());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tasks.set_notifier(tx);

    let id = tasks.spawn("test_job", "测试任务", async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok("计算完成: 100".to_string())
    });

    assert!(id.starts_with("task_"));
    let info = tasks.get(&id).expect("任务必须存在");
    assert_eq!(info.status, TaskStatus::Running);

    // 等待通知到达
    let finished_info = rx.recv().await.expect("必须收到完成通知");
    assert_eq!(finished_info.id, id);
    assert_eq!(finished_info.status, TaskStatus::Completed);
    assert_eq!(
        finished_info.output.as_deref(),
        Some("计算完成: 100")
    );

    let list = tasks.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
}

#[tokio::test]
async fn task_manager_cancel_running_task() {
    let tasks = Arc::new(TaskManager::new());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tasks.set_notifier(tx);

    let id = tasks.spawn("long_job", "长任务", async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok("不该出现".to_string())
    });

    let ok = tasks.cancel(&id);
    assert!(ok, "取消正在运行的任务应该返回 true");

    let finished_info = rx.recv().await.expect("取消必须触发通知");
    assert_eq!(finished_info.id, id);
    assert_eq!(finished_info.status, TaskStatus::Cancelled);

    let current = tasks.get(&id).unwrap();
    assert_eq!(current.status, TaskStatus::Cancelled);
}

#[tokio::test]
async fn manage_task_tool_flow() {
    let ws = std::env::temp_dir();
    let tasks = Arc::new(TaskManager::new());
    let ctx = ToolContext::new(&ws).with_tasks(Arc::clone(&tasks));

    let reg = Registry::with_defaults();

    // 1. 空列表
    let empty_list = reg
        .run("manage_task", json!({ "action": "list" }), &ctx)
        .await
        .unwrap();
    assert!(empty_list.contains("当前没有任何后台任务"));

    // 2. 派发一个任务
    let task_id = tasks.spawn("demo", "演示作业", async {
        Ok("产物数据 XYZ".to_string())
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // 3. 查列表
    let list_out = reg
        .run("manage_task", json!({ "action": "list" }), &ctx)
        .await
        .unwrap();
    assert!(list_out.contains(&task_id));
    assert!(list_out.contains("已完成"));

    // 4. 查详情
    let status_out = reg
        .run(
            "manage_task",
            json!({ "action": "status", "task_id": task_id }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(status_out.contains("产物数据 XYZ"));
}

#[tokio::test]
async fn bash_background_spawn() {
    let ws = std::env::temp_dir();
    let tasks = Arc::new(TaskManager::new());
    let ctx = ToolContext::new(&ws).with_tasks(Arc::clone(&tasks));

    let reg = Registry::with_defaults();
    let out = reg
        .run(
            "bash",
            json!({
                "command": "echo bg_done",
                "background": true
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(out.contains("后台任务已启动 (TaskId: task_"));

    // 等待后台命令执行完成
    tokio::time::sleep(Duration::from_millis(200)).await;

    let list = tasks.list();
    assert!(!list.is_empty());
    let last = list.last().unwrap();
    assert_eq!(last.status, TaskStatus::Completed);
    assert!(last.output.as_ref().unwrap().contains("bg_done"));
}
