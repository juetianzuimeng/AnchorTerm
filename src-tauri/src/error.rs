#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("未连接")]
    NotConnected,

    /// This session_id already has a live transport.
    #[error("该会话已连接，请先断开")]
    AlreadyConnected,

    #[error("会话不存在: {0}")]
    SessionNotFound(String),

    #[error("连接失败: {0}")]
    Connect(String),

    #[error("认证失败: {0}")]
    Auth(String),

    #[error("SSH 错误: {0}")]
    Ssh(String),

    #[error("配置错误: {0}")]
    Config(String),

    #[error("凭据存储错误: {0}")]
    Credential(String),

    #[error("IO 错误: {0}")]
    Io(String),

    #[error("{0}")]
    Message(String),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::NotConnected => "NOT_CONNECTED",
            AppError::AlreadyConnected => "ALREADY_CONNECTED",
            AppError::SessionNotFound(_) => "SESSION_NOT_FOUND",
            AppError::Connect(_) => "CONNECT_FAILED",
            AppError::Auth(_) => "AUTH_FAILED",
            AppError::Ssh(_) => "SSH_ERROR",
            AppError::Config(_) => "CONFIG_ERROR",
            AppError::Credential(_) => "CREDENTIAL_ERROR",
            AppError::Io(_) => "IO_ERROR",
            AppError::Message(_) => "ERROR",
        }
    }
}

impl From<AppError> for String {
    fn from(value: AppError) -> Self {
        // Frontend-friendly: "CODE: message"
        format!("{}: {}", value.code(), value)
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        AppError::Io(value.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        AppError::Config(value.to_string())
    }
}

impl From<keyring::Error> for AppError {
    fn from(value: keyring::Error) -> Self {
        AppError::Credential(value.to_string())
    }
}
