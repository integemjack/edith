//! 套接字的权限：连进来的是谁、能做什么（大脑只有这一个接口，权限都在这里定）。
//! - 令牌定身份：登记里的主令牌（本用户自己开的程序：终端 `edith attach`、桌面端）什么都能做，名字用它报的，
//!   但不能用配置里能力的名字；大脑起配置里的能力时每个发一个专用令牌（EDITH_ACCESS_TOKEN），
//!   拿它连进来的名字就是配置里的名字（别人抢不走），权限按配置的 permissions（默认全开：是你自己配的）；
//! - 中转：能力替别人开的连接（edith-websocket 替远程设备、手机……）用它自己的令牌加 `as` 连：
//!   `{"method":"auth","params":{"token":…,"as":"phone"}}`，名字是 `<能力>-<as>`，权限是 permissions.relay（默认收窄）；
//! - 权限：sense、ask、sessions（none / read / all）、events（订阅事件）、tools（这条连接交来的话，那一轮能用哪些工具：
//!   all / read 只读的 / none / ["read_file", "web_*", …]）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::{Value, json};

/// 一轮能用哪些工具
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Tools {
    #[default]
    All,
    /// 只读的：不改文件、不跑命令（能力的动作写了 readOnly、MCP 工具写了 readOnlyHint 的也算）
    Read,
    None,
    /// 只能用这些（名字，`*` 结尾的按前缀）
    Only(Vec<String>),
}

/// 自带工具里只读的
const READ_ONLY: &[&str] = &[
    "get_current_time",
    "list_directory",
    "read_file",
    "find_files",
    "search_files",
    "search_keywords",
    "web_search",
    "fetch_url",
    "load_tools",
    "use_skill",
];

impl Tools {
    fn parse(value: &Value) -> Option<Tools> {
        Some(match value {
            Value::String(text) if text == "all" => Tools::All,
            Value::String(text) if text == "read" => Tools::Read,
            Value::String(text) if text == "none" => Tools::None,
            Value::Array(items) => Tools::Only(items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect()),
            _ => return None,
        })
    }

    fn to_json(&self) -> Value {
        match self {
            Tools::All => json!("all"),
            Tools::Read => json!("read"),
            Tools::None => json!("none"),
            Tools::Only(names) => json!(names),
        }
    }

    /// 能不能用这个工具（read_only：能力 / MCP 说它只读）
    pub fn allows(&self, name: &str, read_only: bool) -> bool {
        match self {
            Tools::All => true,
            Tools::Read => read_only || READ_ONLY.contains(&name),
            Tools::None => false,
            Tools::Only(names) => {
                names.iter().any(|pattern| pattern.strip_suffix('*').map_or(pattern == name, |prefix| name.starts_with(prefix)))
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Tools::All => "全部工具".into(),
            Tools::Read => "只读的工具".into(),
            Tools::None => "不能用工具".into(),
            Tools::Only(names) => format!("只能用 {}", names.join("、")),
        }
    }
}

/// 会话：看不了 / 只看 / 看和改（新建、切焦点、停下）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sessions {
    None,
    Read,
    All,
}

/// 一条连接能做什么
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub sense: bool,
    pub ask: bool,
    pub sessions: Sessions,
    pub events: bool,
    pub tools: Tools,
}

impl Grant {
    pub fn full() -> Grant {
        Grant { sense: true, ask: true, sessions: Sessions::All, events: true, tools: Tools::All }
    }

    /// 中转进来的（远程设备、手机）默认：能说话、能问、能看会话和事件，它交来的话那一轮只用只读的工具
    pub fn relay() -> Grant {
        Grant { sense: true, ask: true, sessions: Sessions::Read, events: true, tools: Tools::Read }
    }

    /// 配置里写的盖在 base 上（没写的照 base）
    pub fn overlay(base: Grant, value: Option<&Value>) -> Grant {
        let Some(object) = value.and_then(Value::as_object) else { return base };
        let flag = |key: &str, default: bool| object.get(key).and_then(Value::as_bool).unwrap_or(default);
        Grant {
            sense: flag("sense", base.sense),
            ask: flag("ask", base.ask),
            sessions: match object.get("sessions") {
                Some(Value::String(text)) if text == "all" => Sessions::All,
                Some(Value::String(text)) if text == "read" => Sessions::Read,
                Some(Value::String(text)) if text == "none" => Sessions::None,
                Some(Value::Bool(enabled)) => {
                    if *enabled {
                        Sessions::All
                    } else {
                        Sessions::None
                    }
                }
                _ => base.sessions,
            },
            events: flag("events", base.events),
            tools: object.get("tools").and_then(Tools::parse).unwrap_or(base.tools),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "sense": self.sense,
            "ask": self.ask,
            "sessions": match self.sessions { Sessions::None => "none", Sessions::Read => "read", Sessions::All => "all" },
            "events": self.events,
            "tools": self.tools.to_json(),
        })
    }
}

/// 连进来的是谁
#[derive(Debug, Clone)]
pub struct Authority {
    /// 名字定死了（配置里的能力、它中转的）；None：主令牌的连接，用它报的名字
    pub name: Option<String>,
    /// 中转的子连接（名字 `<能力>-<as>`）
    pub relayed: bool,
    pub grant: Grant,
}

/// 配置里能力的专用令牌：令牌 → (能力名, 起它时的配置)（大脑这次运行里不变，能力断了重连还用它；去掉能力时作废）
static TOKENS: LazyLock<Mutex<HashMap<String, (String, Value)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 起这个能力时交给它的令牌（记下这次的配置：连进来时按它的 permissions 给权限）
pub fn token_for(name: &str, config: &Value) -> String {
    let mut tokens = TOKENS.lock().unwrap();
    if let Some((token, entry)) = tokens.iter_mut().find(|(_, (owner, _))| owner == name) {
        entry.1 = config.clone();
        return token.clone();
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    tokens.insert(token.clone(), (name.to_string(), config.clone()));
    token
}

/// 能力去掉了：它的令牌作废
pub fn revoke(name: &str) {
    TOKENS.lock().unwrap().retain(|_, (owner, _)| owner != name);
}

/// auth 的令牌 → 身份；不认识的令牌返回 None
pub fn identify(token: &str, owner: &str, relay_as: Option<&str>) -> Option<Authority> {
    if !owner.is_empty() && token == owner {
        return Some(Authority { name: None, relayed: false, grant: Grant::full() });
    }
    let (name, config) = TOKENS.lock().unwrap().get(token).cloned()?;
    let permissions = config.get("permissions");
    let own = Grant::overlay(Grant::full(), permissions);
    match relay_as.map(crate::capabilities::clean_name).filter(|child| !child.is_empty()) {
        None => Some(Authority { name: Some(name), relayed: false, grant: own }),
        Some(child) => {
            // 中转的不会比替它中转的能力权限大
            let relay = Grant::overlay(Grant::relay(), permissions.and_then(|permissions| permissions.get("relay")));
            Some(Authority { name: Some(format!("{name}-{child}")), relayed: true, grant: narrower(relay, &own) })
        }
    }
}

/// 两份权限取交集
fn narrower(grant: Grant, limit: &Grant) -> Grant {
    let tools = match (&grant.tools, &limit.tools) {
        (_, Tools::None) | (Tools::None, _) => Tools::None,
        (tools, Tools::All) => tools.clone(),
        (Tools::All, limit) => limit.clone(),
        (Tools::Read, Tools::Read) => Tools::Read,
        (Tools::Only(names), _) | (_, Tools::Only(names)) => Tools::Only(names.clone()),
    };
    Grant {
        sense: grant.sense && limit.sense,
        ask: grant.ask && limit.ask,
        sessions: grant.sessions.min(limit.sessions),
        events: grant.events && limit.events,
        tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_grants() {
        let config = json!({ "name": "phone-relay", "permissions": { "sessions": "read", "relay": { "tools": ["web_*"] } } });
        let token = token_for("phone-relay", &config);
        assert_eq!(token, token_for("phone-relay", &config));
        let own = identify(&token, "owner", None).unwrap();
        assert_eq!(own.name.as_deref(), Some("phone-relay"));
        assert_eq!(own.grant.sessions, Sessions::Read);
        assert_eq!(own.grant.tools, Tools::All);
        let relayed = identify(&token, "owner", Some("my phone")).unwrap();
        assert_eq!(relayed.name.as_deref(), Some("phone-relay-my_phone"));
        assert!(relayed.grant.tools.allows("web_search", false) && !relayed.grant.tools.allows("run_shell", false));
        assert!(identify("owner", "owner", None).unwrap().name.is_none());
        assert!(identify("nope", "owner", None).is_none());
        revoke("phone-relay");
        assert!(identify(&token, "owner", None).is_none());
        assert!(Tools::Read.allows("read_file", false) && !Tools::Read.allows("write_file", false) && Tools::Read.allows("x__look", true));
    }
}
