//! 统一错误类型 + axum 响应映射。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lumen_protocol::ApiError;

/// 服务端错误。每个变体映射到一个 HTTP 状态码 + 机器可读 code。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// 邮箱已被注册。
    #[error("邮箱已被注册")]
    EmailTaken,
    /// 邮箱或密码错误。
    #[error("邮箱或密码错误")]
    InvalidCredentials,
    /// 账户不存在（客户端据此对新邮箱自动注册，见 lumen-app/src/cloud.rs）。
    #[error("账户不存在")]
    UserNotFound,
    /// 未授权（缺 token / token 无效或过期）。
    #[error("未授权")]
    Unauthorized,
    /// 设备不存在或不属于本账户。
    #[error("设备不存在")]
    DeviceNotFound,
    /// 请求参数无效。
    #[error("请求参数无效: {0}")]
    BadRequest(String),
    /// 达到某项资源上限（片 11：设备数）。
    ///
    /// **与 [`AppError::BadRequest`] 分开**：它不是「你发的东西不对」，而是「你没错，
    /// 但这里满了」。客户端要给出的引导完全不同（去删台旧设备 vs 检查请求）。
    #[error("{0}")]
    LimitReached(String),
    /// 请求过于频繁（片 11：登录 / 注册节流）。
    ///
    /// `retry_after_secs` 会同时进响应体与 `Retry-After` 头——只给一句「太频繁」而不说
    /// 等多久，客户端只能盲目重试，那正是节流想挡住的行为。
    #[error("请求过于频繁，请 {retry_after_secs} 秒后再试")]
    TooManyRequests {
        /// 建议的重试等待秒数。
        retry_after_secs: u64,
    },
    /// 数据库错误（连接池 / 查询）。
    #[error("数据库错误: {0}")]
    Db(String),
    /// 内部错误。
    #[error("内部错误: {0}")]
    Internal(String),
}

impl From<tokio_postgres::Error> for AppError {
    fn from(e: tokio_postgres::Error) -> Self {
        AppError::Db(e.to_string())
    }
}

impl From<deadpool_postgres::PoolError> for AppError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        AppError::Db(e.to_string())
    }
}

impl AppError {
    /// 映射到 (HTTP 状态码, 机器可读 code)。
    fn parts(&self) -> (StatusCode, &'static str) {
        match self {
            AppError::EmailTaken => (StatusCode::CONFLICT, "email_taken"),
            AppError::InvalidCredentials => (StatusCode::UNAUTHORIZED, "invalid_credentials"),
            AppError::UserNotFound => (StatusCode::NOT_FOUND, "user_not_found"),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            AppError::DeviceNotFound => (StatusCode::NOT_FOUND, "device_not_found"),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            AppError::LimitReached(_) => (StatusCode::CONFLICT, "limit_reached"),
            AppError::TooManyRequests { .. } => {
                (StatusCode::TOO_MANY_REQUESTS, "too_many_requests")
            }
            AppError::Db(_) => (StatusCode::INTERNAL_SERVER_ERROR, "db_error"),
            AppError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.parts();
        // 节流要带 `Retry-After`：标准头，代理与客户端库都认，而只靠响应体里那句话
        // 需要每个客户端自己解析文案。
        let retry_after = match &self {
            AppError::TooManyRequests { retry_after_secs } => Some(*retry_after_secs),
            _ => None,
        };
        // 5xx 不向客户端外泄底层细节（DB/内部错误），仅进服务端日志。
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("服务端错误: {self}");
            "服务器内部错误".to_string()
        } else {
            self.to_string()
        };
        let body = (status, Json(ApiError::new(code, message))).into_response();
        let Some(secs) = retry_after else { return body };
        let mut body = body;
        if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
            body.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, v);
        }
        body
    }
}

/// handler 返回别名。
pub type AppResult<T> = Result<T, AppError>;
