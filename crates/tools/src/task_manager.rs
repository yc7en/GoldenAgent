use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use std::future::Future;
use goldenagent_core::{Result, TaskInfo, TaskStatus};
use tokio::sync::mpsc;

static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct TaskRecord {
    info: TaskInfo,
    abort_handle: Option<tokio::task::AbortHandle>,
}

/// 全局后台作业管理器（B21）。
///
/// 遵循 ARCHITECTURE A-16：
/// - 维护后台任务清单与执行状态；
/// - 提供异步解耦的派生与中止能力；
/// - 支持完成通知通道，供响应式唤醒（Reactive Wakeup）消费。
#[derive(Clone)]
pub struct TaskManager {
    records: Arc<Mutex<HashMap<String, TaskRecord>>>,
    notifier: Arc<Mutex<Option<mpsc::UnboundedSender<TaskInfo>>>>,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskManager")
            .field("task_count", &self.records.lock().unwrap().len())
            .finish()
    }
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            records: Arc::new(Mutex::new(HashMap::new())),
            notifier: Arc::new(Mutex::new(None)),
        }
    }

    /// 设置任务完成时的异步通知通道。
    pub fn set_notifier(&self, tx: mpsc::UnboundedSender<TaskInfo>) {
        if let Ok(mut slot) = self.notifier.lock() {
            *slot = Some(tx);
        }
    }

    /// 生成唯一 TaskId。
    pub fn next_id(&self) -> String {
        let seq = TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("task_{seq}")
    }

    /// 在后台派发一个异步长耗时作业。
    pub fn spawn<F>(&self, kind: &str, description: &str, fut: F) -> String
    where
        F: Future<Output = Result<String>> + Send + 'static,
    {
        let id = self.next_id();
        let created_at_ms = now_ms();

        let initial_info = TaskInfo {
            id: id.clone(),
            kind: kind.to_string(),
            description: description.to_string(),
            status: TaskStatus::Running,
            created_at_ms,
            finished_at_ms: None,
            output: None,
            error: None,
        };

        let records = Arc::clone(&self.records);
        let notifier = Arc::clone(&self.notifier);
        let task_id = id.clone();

        let join_handle = tokio::spawn(async move {
            let res = fut.await;
            let finished_at_ms = now_ms();

            let final_info = {
                let mut map = records.lock().unwrap();
                if let Some(record) = map.get_mut(&task_id) {
                    match res {
                        Ok(output) => {
                            record.info.status = TaskStatus::Completed;
                            record.info.output = Some(output);
                        }
                        Err(err) => {
                            record.info.status = TaskStatus::Failed;
                            record.info.error = Some(err.to_string());
                        }
                    }
                    record.info.finished_at_ms = Some(finished_at_ms);
                    record.info.clone()
                } else {
                    return;
                }
            };

            // 触发完成通知（供响应式唤醒使用）
            if let Ok(slot) = notifier.lock() {
                if let Some(tx) = slot.as_ref() {
                    let _ = tx.send(final_info);
                }
            }
        });

        {
            let mut map = self.records.lock().unwrap();
            map.insert(
                id.clone(),
                TaskRecord {
                    info: initial_info,
                    abort_handle: Some(join_handle.abort_handle()),
                },
            );
        }

        id
    }

    /// 查询全部后台作业列表，按创建时间正序排列。
    pub fn list(&self) -> Vec<TaskInfo> {
        let map = self.records.lock().unwrap();
        let mut list: Vec<TaskInfo> = map.values().map(|r| r.info.clone()).collect();
        list.sort_by_key(|t| t.created_at_ms);
        list
    }

    /// 查询特定作业的状态与输出。
    pub fn get(&self, id: &str) -> Option<TaskInfo> {
        let map = self.records.lock().unwrap();
        map.get(id).map(|r| r.info.clone())
    }

    /// 中止/取消正在执行的后台作业。
    pub fn cancel(&self, id: &str) -> bool {
        let mut map = self.records.lock().unwrap();
        if let Some(record) = map.get_mut(id) {
            if record.info.status == TaskStatus::Running {
                if let Some(handle) = record.abort_handle.take() {
                    handle.abort();
                }
                record.info.status = TaskStatus::Cancelled;
                record.info.finished_at_ms = Some(now_ms());
                record.info.error = Some("任务已被用户或主 Agent 终止".to_string());

                let final_info = record.info.clone();
                drop(map);

                if let Ok(slot) = self.notifier.lock() {
                    if let Some(tx) = slot.as_ref() {
                        let _ = tx.send(final_info);
                    }
                }
                return true;
            }
        }
        false
    }
}
