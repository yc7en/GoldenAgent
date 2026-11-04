use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("provider 出错: {0}")]
    Provider(String),

    /// 配置本身有问题（缺 key、JSON 写坏、字段非法）。
    ///
    /// 与 `Provider` 分开是因为处理方式不同：配置错误要人改，
    /// provider 错误可能是厂商抖动，重试一下就好。
    #[error("配置出错: {0}")]
    Config(String),

    #[error("工具出错: {0}")]
    Tool(String),

    #[error("会话出错: {0}")]
    Session(String),

    #[error("策略出错: {0}")]
    Policy(String),

    #[error("已取消")]
    Cancelled,

    #[error("超出预算")]
    BudgetExceeded,
}

pub type Result<T> = std::result::Result<T, Error>;
