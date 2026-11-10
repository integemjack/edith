//! 顺手记下的服务用量（同 Friday 的 ServiceUsage「顺手记」、Qt 的 ServiceUsage::noteSuccess / noteQuota）：
//! 每次请求成功记下成功时间和响应头里的限流余量，额度用完 / 余额不足时记下什么时候、哪一类、服务说几点恢复。
//! 存 `~/.edith/service-usage.json`，格式、键和 Friday 的一样（`{"services": {"<接口地址小写去掉末尾 />|<密钥名>": {…}}}`），
//! edith-friday 回 services.usage 时读它（Friday 客户端的设置页、模型胶囊、菜单显示「额度用完」、限流余量）；
//! 记了就推一个 `usage` 事件（observedKey、observed：额度用完 / 用完后头一次成功时是 true），网关转成 Friday 的 serviceUsage 事件。

use std::sync::Mutex;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use crate::home;

/// 读改写这个文件时拿着（同一个大脑里几段请求同时记）
static LOCK: Mutex<()> = Mutex::new(());

fn path() -> std::path::PathBuf {
    home::dir().join("service-usage.json")
}

/// 记录的键：接口地址去掉末尾的 /、小写，加「|密钥名」（== Friday ServiceUsage.observedKey）
pub fn observed_key(base_url: &str, secret_name: &str) -> String {
    let mut base = base_url.trim().to_lowercase();
    while base.ends_with('/') {
        base.pop();
    }
    format!("{base}|{secret_name}")
}

fn iso(date: DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse(text: Option<&str>) -> Option<DateTime<Utc>> {
    text.and_then(|text| DateTime::parse_from_rfc3339(text).ok()).map(|date| date.with_timezone(&Utc))
}

/// 响应头里的限流余量：OpenAI 系 x-ratelimit-{limit,remaining,reset}-{requests,tokens}、
/// Anthropic 系 anthropic-ratelimit-{requests,tokens}-{limit,remaining,reset}（== Friday ServiceUsage.rateLimitWindows）
pub fn rate_limit_windows(header: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Vec<Value> {
    let number = |name: &str| header(name).and_then(|value| value.trim().parse::<f64>().ok());
    let window = |title: &str, limit: &str, remaining: &str, reset: &str| -> Option<Value> {
        let left = number(remaining)?;
        let mut window = Map::new();
        window.insert("title".into(), json!(title));
        if let Some(limit) = number(limit) {
            window.insert("limit".into(), json!(limit));
        }
        window.insert("remaining".into(), json!(left));
        if let Some(seconds) = header(reset)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .and_then(|text| friday_core::quota::reset_value(&text, now))
        {
            window.insert("resetAt".into(), json!(iso(now + chrono::Duration::milliseconds((seconds * 1000.0).round() as i64))));
        }
        Some(Value::Object(window))
    };
    let mut windows: Vec<Value> = [
        window("请求", "x-ratelimit-limit-requests", "x-ratelimit-remaining-requests", "x-ratelimit-reset-requests"),
        window("tokens", "x-ratelimit-limit-tokens", "x-ratelimit-remaining-tokens", "x-ratelimit-reset-tokens"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if windows.is_empty() {
        windows.extend(
            [
                window(
                    "请求",
                    "anthropic-ratelimit-requests-limit",
                    "anthropic-ratelimit-requests-remaining",
                    "anthropic-ratelimit-requests-reset",
                ),
                window(
                    "tokens",
                    "anthropic-ratelimit-tokens-limit",
                    "anthropic-ratelimit-tokens-remaining",
                    "anthropic-ratelimit-tokens-reset",
                ),
            ]
            .into_iter()
            .flatten(),
        );
        if windows.len() < 2 {
            windows.extend(window(
                "输入 tokens",
                "anthropic-ratelimit-input-tokens-limit",
                "anthropic-ratelimit-input-tokens-remaining",
                "anthropic-ratelimit-input-tokens-reset",
            ));
        }
    }
    windows
}

/// 改一条记录、写回文件；返回改完的记录
fn update(path: &std::path::Path, key: &str, change: impl FnOnce(&mut Map<String, Value>)) -> Value {
    let _guard = LOCK.lock().unwrap();
    let mut file = home::read_json(path).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if !file["services"].is_object() {
        file["services"] = json!({});
    }
    let services = file["services"].as_object_mut().unwrap();
    let entry = services.entry(key.to_string()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    change(entry.as_object_mut().unwrap());
    let updated = entry.clone();
    let _ = home::write_json(path, &file);
    updated
}

/// 一次请求成功了：记下成功时间和响应头里的限流余量。返回 true = 这是额度用完之后头一次成功（恢复了）
pub fn note_success(base_url: &str, secret_name: &str, header: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> bool {
    note_success_at(&path(), base_url, secret_name, header, now)
}

fn note_success_at(
    path: &std::path::Path,
    base_url: &str,
    secret_name: &str,
    header: &dyn Fn(&str) -> Option<String>,
    now: DateTime<Utc>,
) -> bool {
    let windows = rate_limit_windows(header, now);
    let mut recovered = false;
    update(path, &observed_key(base_url, secret_name), |entry| {
        let quota = parse(entry.get("quotaAt").and_then(Value::as_str));
        let ok = parse(entry.get("okAt").and_then(Value::as_str));
        recovered = quota.is_some_and(|quota| ok.is_none_or(|ok| ok < quota));
        entry.insert("okAt".into(), json!(iso(now)));
        if !windows.is_empty() {
            entry.insert("limits".into(), Value::Array(windows));
            entry.insert("limitsAt".into(), json!(iso(now)));
        }
    });
    recovered
}

/// 额度用完 / 余额不足了：kind 是 usage / balance，resume_at 是服务说的恢复时间（没说为 None）
pub fn note_quota(base_url: &str, secret_name: &str, kind: &str, resume_at: Option<DateTime<Utc>>, now: DateTime<Utc>) {
    note_quota_at(&path(), base_url, secret_name, kind, resume_at, now);
}

fn note_quota_at(
    path: &std::path::Path,
    base_url: &str,
    secret_name: &str,
    kind: &str,
    resume_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) {
    update(path, &observed_key(base_url, secret_name), |entry| {
        entry.insert("quotaAt".into(), json!(iso(now)));
        entry.insert("quotaKind".into(), json!(kind));
        match resume_at {
            Some(date) => entry.insert("resumeAt".into(), json!(iso(date))),
            None => entry.remove("resumeAt"),
        };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_windows_and_recovery() {
        assert_eq!(
            observed_key(" https://Open.BigModel.cn/api/coding/paas/v4/ ", "ZHIPU_API_KEY"),
            "https://open.bigmodel.cn/api/coding/paas/v4|ZHIPU_API_KEY"
        );
        let now = friday_core::quota::utc(2026, 11, 10, 10, 0, 0);
        let headers = |name: &str| match name {
            "x-ratelimit-limit-requests" => Some("100".to_string()),
            "x-ratelimit-remaining-requests" => Some("42".to_string()),
            "x-ratelimit-reset-requests" => Some("6m0s".to_string()),
            _ => None,
        };
        let windows = rate_limit_windows(&headers, now);
        assert_eq!(windows, vec![json!({ "title": "请求", "limit": 100.0, "remaining": 42.0, "resetAt": "2026-11-10T10:06:00Z" })]);
        assert!(rate_limit_windows(&|_| None, now).is_empty());

        let home = std::env::temp_dir().join(format!("edith-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let file = home.join("service-usage.json");
        let none = |_: &str| None;
        assert!(!note_success_at(&file, "https://x/v1", "K", &none, now), "从没用完过不算恢复");
        note_quota_at(&file, "https://x/v1", "K", "usage", Some(now + chrono::Duration::hours(1)), now + chrono::Duration::seconds(1));
        assert!(note_success_at(&file, "https://x/v1/", "K", &headers, now + chrono::Duration::seconds(2)), "用完之后头一次成功");
        assert!(!note_success_at(&file, "https://x/v1", "K", &none, now + chrono::Duration::seconds(3)));
        let file: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let entry = &file["services"]["https://x/v1|K"];
        assert_eq!((entry["quotaKind"].as_str(), entry["resumeAt"].as_str()), (Some("usage"), Some("2026-11-10T11:00:00Z")));
        assert_eq!(entry["okAt"], "2026-11-10T10:00:03Z");
        assert_eq!(entry["limits"][0]["remaining"], 42.0, "没给限流头的那次不清掉上一次记的");
        let _ = std::fs::remove_dir_all(&home);
    }
}
