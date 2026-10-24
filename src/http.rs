//! 发 HTTP（大脑自己发，不经过任何桌面端）：模型的流式请求按 SSE 一行行读；走环境变量里的代理（HTTPS_PROXY / ALL_PROXY / NO_PROXY）

use std::io::{BufRead, BufReader};
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::Value;

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        // 思考很久的模型首个 token 可能要几分钟；之后每块之间不会这么久
        .timeout_recv_response(Some(Duration::from_secs(600)))
        .build()
        .into()
});

pub fn agent() -> &'static ureq::Agent {
    &AGENT
}

#[derive(Debug)]
pub enum Failure {
    /// 服务端回了错误状态：状态码、响应正文、Retry-After（秒）
    Status { status: u16, body: String, retry_after: Option<f64> },
    /// 连不上、读到一半断了
    Network(String),
}

impl Failure {
    pub fn message(&self) -> String {
        match self {
            Failure::Status { status, body, .. } => format!("HTTP {status}：{}", body.chars().take(600).collect::<String>()),
            Failure::Network(message) => format!("网络出错：{message}"),
        }
    }
}

/// POST 一个 JSON、按 SSE 读回来：每个 `data:` 负载交给 on_data（返回 false 就不读了）；读到 [DONE] 返回 Ok(true)，流自己结束 Ok(false)
pub fn post_sse(url: &str, api_key: &str, body: &Value, mut on_data: impl FnMut(&str) -> bool) -> Result<bool, Failure> {
    let mut request = agent().post(url).header("Content-Type", "application/json").header("Accept", "text/event-stream");
    if !api_key.is_empty() && api_key != "none" {
        request = request.header("Authorization", format!("Bearer {api_key}"));
    }
    let mut response = request.send(body.to_string()).map_err(|error| Failure::Network(error.to_string()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let retry_after =
            response.headers().get("retry-after").and_then(|value| value.to_str().ok()).and_then(friday_core::chat_request::header_seconds);
        let body = response.body_mut().read_to_string().unwrap_or_default();
        return Err(Failure::Status { status, body, retry_after });
    }
    let mut reader = BufReader::new(response.body_mut().as_reader());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(false),
            Ok(_) => {}
            Err(error) => return Err(Failure::Network(error.to_string())),
        }
        let Some(data) = line.trim_end_matches(['\r', '\n']).strip_prefix("data:") else { continue };
        let data = data.trim_start();
        if data == "[DONE]" {
            return Ok(true);
        }
        if !on_data(data) {
            return Ok(false);
        }
    }
}
