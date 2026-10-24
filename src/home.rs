//! 数据目录（`~/.edith`，`EDITH_HOME` 换地方）和大脑用的模型（`config.json` + 环境变量）

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// 用户的家目录（Unix 的 HOME、Windows 的 USERPROFILE）
pub fn user_home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn dir() -> PathBuf {
    match std::env::var_os("EDITH_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => user_home().join(".edith"),
    }
}

pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// 先写临时文件再换名（写到一半退出不留半份）
pub fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("建不了 {}：{error}", parent.display()))?;
    }
    friday_core::writer::write_now(path, text.as_bytes()).map_err(|error| format!("写不了 {}：{error}", path.display()))
}

/// 大脑用的模型（OpenAI 兼容接口）
#[derive(Debug, Clone)]
pub struct Model {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// none / toggle / effort / thinkingType（== Friday 的 ThinkingSupport；不写按地址推断）
    pub thinking: String,
    pub enable_thinking: bool,
    pub temperature: f64,
    pub supports_vision: bool,
    /// 额外请求参数（JSON 对象的文本，盖掉同名的）
    pub extra_body: String,
}

impl Model {
    /// config.json 的值，环境变量 EDITH_BASE_URL / EDITH_API_KEY / EDITH_MODEL 盖掉同名的
    pub fn load() -> Result<Model, String> {
        let path = dir().join("config.json");
        let config = read_json(&path).unwrap_or_else(|| json!({}));
        let text = |key: &str, variable: &str| {
            std::env::var(variable)
                .ok()
                .filter(|value| !value.is_empty())
                .or_else(|| config.get(key).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default()
        };
        let base_url = text("baseURL", "EDITH_BASE_URL").trim_end_matches('/').to_string();
        let model = text("model", "EDITH_MODEL");
        if base_url.is_empty() || model.is_empty() {
            return Err(format!(
                "还不知道用哪个模型：在 {} 里写 {{\"baseURL\": \"https://…/v1\", \"apiKey\": \"…\", \"model\": \"…\"}}，\
                 或设环境变量 EDITH_BASE_URL、EDITH_API_KEY、EDITH_MODEL",
                path.display()
            ));
        }
        let thinking = config
            .get("thinking")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| friday_core::engines::inferred_thinking(&base_url, false).to_string());
        Ok(Model {
            api_key: text("apiKey", "EDITH_API_KEY"),
            thinking,
            enable_thinking: config.get("enableThinking").and_then(Value::as_bool).unwrap_or(false),
            temperature: config.get("temperature").and_then(Value::as_f64).unwrap_or(0.7),
            supports_vision: config.get("supportsVision").and_then(Value::as_bool).unwrap_or(false),
            extra_body: config
                .get("extraBody")
                .map(|extra| if extra.is_string() { extra.as_str().unwrap().to_string() } else { extra.to_string() })
                .unwrap_or_default(),
            base_url,
            model,
        })
    }

    /// 记「不接受的参数」、上下文上限用的键
    pub fn engine_id(&self) -> String {
        format!("edith|{}|{}", self.base_url, self.model)
    }
}
