//! 第 0 层：领域类型 + 对外事件契约。
//!
//! 这个 crate 只有类型，没有 IO、没有运行时、没有 UI。
//! 上层所有 crate 都可以依赖它，它不依赖任何人。
//!
//! 关键约定：[`Event`] 是整个系统对外唯一的状态出口。
//! 前端、CLI、日志、测试全部只消费事件，不允许伸手去读内部结构。

mod error;
mod event;
mod message;
mod task;
mod tool;

pub use error::{Error, Result};
pub use event::{Event, StopReason};
pub use message::{ContentBlock, Message, Role, ToolCall, ToolResult};
pub use task::{TaskInfo, TaskStatus};
pub use tool::{ToolKind, ToolSpec};
