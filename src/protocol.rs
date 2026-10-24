//! 能力协议的消息写法（docs/CAPABILITY-PROTOCOL.md）：JSON-RPC 2.0，经 stdio 一行一条

use serde_json::{Value, json};

/// 协议版本（initialize 里双方都写）
pub const VERSION: &str = "1";

// JSON-RPC 2.0 的错误码
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
// EDITH 自己的
/// 还没握手（EDITH 发 initialize、能力回话之前）
pub const NOT_INITIALIZED: i64 = -32001;
/// 没有这个会话
pub const NO_SESSION: i64 = -32002;
/// 大脑用哪个模型还没设
pub const NO_MODEL: i64 = -32003;
/// 被停下了（动作被 act.cancel、等的那一轮被停下）
pub const CANCELLED: i64 = -32004;

pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

pub fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// 一条消息是什么
pub enum Message<'a> {
    Request { id: &'a Value, method: &'a str, params: &'a Value },
    Notification { method: &'a str, params: &'a Value },
    Response { id: &'a Value, outcome: Result<&'a Value, &'a Value> },
    Invalid,
}

static NULL: Value = Value::Null;

pub fn classify(message: &Value) -> Message<'_> {
    let Some(object) = message.as_object() else { return Message::Invalid };
    let params = object.get("params").unwrap_or(&NULL);
    match (object.get("method").and_then(Value::as_str), object.get("id").filter(|id| !id.is_null())) {
        (Some(method), Some(id)) => Message::Request { id, method, params },
        (Some(method), None) => Message::Notification { method, params },
        (None, Some(id)) => match (object.get("result"), object.get("error")) {
            (_, Some(error)) => Message::Response { id, outcome: Err(error) },
            (Some(result), None) => Message::Response { id, outcome: Ok(result) },
            (None, None) => Message::Invalid,
        },
        (None, None) => Message::Invalid,
    }
}
