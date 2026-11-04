//! 取消标记。
//!
//! 故意做得很薄：一个 `AtomicBool`，外加一个"变了就叫我"的通知。
//!
//! 用处分两种：
//!
//! - **轮次边界 / 工具边界**：直接 `is_cancelled()` 判断，不做抢占式中断——
//!   工具执行到一半被打断会留下半个文件，比慢一点更糟；
//! - **等模型回话的时候**：用 `cancelled().await` 挂起。
//!   模型一次可能要说几十秒，这期间用户按了停止必须立刻生效，
//!   不能再等它把话说完。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Debug, Clone, Default)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 挂起直到被取消。已经取消过就立即返回。
    ///
    /// 先注册再复查，避免"注册之前就被取消"这个丢唤醒的窗口——
    /// `notify_waiters` 只唤醒已经注册的等待者。
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }

        let waiter = self.notify.notified();
        tokio::pin!(waiter);
        waiter.as_mut().enable();

        if self.is_cancelled() {
            return;
        }
        waiter.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_returns_immediately_when_already_set() {
        let cancel = Cancellation::new();
        cancel.cancel();
        // 挂住就说明实现错了，所以直接 await
        cancel.cancelled().await;
    }

    #[tokio::test]
    async fn wakes_up_when_cancelled_later() {
        let cancel = Cancellation::new();
        let trigger = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            trigger.cancel();
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), cancel.cancelled())
            .await
            .expect("取消应唤醒等待者");
    }
}
