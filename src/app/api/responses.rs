use crate::api::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

pub(crate) fn encode_success(id: String, result: ResponseResult) -> String {
    // APP-012：序列化理论上不会失败（schema 全是派生 Serialize）；真失败也
    // 不能 panic——回最小错误体并记一条错误日志。
    match serde_json::to_string(&SuccessResponse { id, result }) {
        Ok(encoded) => encoded,
        Err(err) => serialization_failure(None, &err),
    }
}

/// 任意可序列化响应包的编码：序列化失败时回最小错误体而不是 panic。
pub(crate) fn encode_response_value(value: impl serde::Serialize) -> String {
    match serde_json::to_string(&value) {
        Ok(encoded) => encoded,
        Err(err) => serialization_failure(None, &err),
    }
}

fn serialization_failure(id: Option<&str>, err: &serde_json::Error) -> String {
    tracing::error!(err = %err, "failed to serialize API response");
    let id = id.unwrap_or_default();
    format!(
        "{{\"id\":{},\"error\":{{\"code\":\"internal_error\",\"message\":\"response serialization failed\"}}}}",
        serde_json::Value::String(id.to_owned())
    )
}

pub(crate) fn encode_error(id: String, code: &str, message: impl Into<String>) -> String {
    encode_error_body(
        id,
        ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    )
}

pub(super) fn encode_error_body(id: String, error: ErrorBody) -> String {
    match serde_json::to_string(&ErrorResponse { id, error }) {
        Ok(encoded) => encoded,
        Err(err) => serialization_failure(None, &err),
    }
}
