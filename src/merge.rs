//! 发给模型的工具合并（同 Friday v2.90 的 ToolMerge）：同一组的合成一个，参数是各成员的并集，靠参数本身选成员——
//! search_files 给 keywords 是按关键词搜（search_keywords），find_files 不给 pattern 是列目录（list_directory），
//! delegate_task 给 tasks 是并联派（run_parallel_tasks）。
//! 执行、权限、批准、钩子、卡片仍按原来的细分工具：模型调进来时 internal 换回。存档存的是原样（模型调的什么就是什么），
//! 不用像 Friday 那样还原给模型。旧名照样能调；限定工具的列表（连接的、现成子智能体的）里写合并名也认。
//! Friday 其余几组（造工具、管理工具 / MCP / 提供方 / 命令）在 edith-friday 网关那边合；wait_agents EDITH 本来就没有。

use serde_json::{Map, Value, json};

/// 合并名 → 成员（第一个是主的：说明跟它；合成的放在组里最先出现的那个的位置）
const GROUPS: &[(&str, &[&str])] = &[
    ("search_files", &["search_files", "search_keywords"]),
    ("find_files", &["find_files", "list_directory"]),
    ("delegate_task", &["delegate_task", "run_parallel_tasks"]),
];

fn name_of(spec: &Value) -> &str {
    spec["function"]["name"].as_str().unwrap_or("")
}

fn filled(arguments: &Map<String, Value>, key: &str) -> bool {
    match arguments.get(key) {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

fn group_of(name: &str) -> Option<&'static (&'static str, &'static [&'static str])> {
    GROUPS.iter().find(|(_, members)| members.contains(&name))
}

/// 模型调进来的合并名 → 内部工具名（参数照用，不用改）；不是合并名的原样
pub fn internal(name: &str, arguments: &Map<String, Value>) -> String {
    match name {
        "search_files" if filled(arguments, "keywords") && !filled(arguments, "pattern") => "search_keywords",
        "find_files" if !filled(arguments, "pattern") => "list_directory",
        "delegate_task" if arguments.get("tasks").is_some_and(Value::is_array) => "run_parallel_tasks",
        _ => name,
    }
    .to_string()
}

/// 内部工具所在组的合并名（不在组里的是 None）
pub fn merged_name(name: &str) -> Option<String> {
    group_of(name).map(|(group, _)| group.to_string())
}

/// 限定工具的列表里的一项（工具名或合并名）管不管得着这个内部工具
pub fn covers(listed: &str, name: &str) -> bool {
    listed == name || GROUPS.iter().any(|(group, members)| *group == listed && members.contains(&name))
}

/// 这个工具不单独给了：同组有别的成员一直给（按需的清单里不列、取回了也不单独给）
pub fn absorbed(name: &str, given: &[String]) -> bool {
    group_of(name).is_some_and(|(_, members)| members.iter().any(|member| *member != name && given.iter().any(|tool| tool == member)))
}

/// 一组的合成定义：主成员（没有就第一个在的）打底，名字换成组名；参数取并集、必填的取交集
fn merged(group: &str, present: &[&Value]) -> Value {
    let mut result = present[0].clone();
    let mut properties = Map::new();
    let mut required: Option<Vec<String>> = None;
    for spec in present {
        let parameters = &spec["function"]["parameters"];
        for (key, value) in parameters["properties"].as_object().into_iter().flatten() {
            properties.entry(key.clone()).or_insert_with(|| value.clone());
        }
        let needed: Vec<String> =
            parameters["required"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        required = Some(match required {
            None => needed,
            Some(previous) => previous.into_iter().filter(|key| needed.contains(key)).collect(),
        });
    }
    let mut required = required.unwrap_or_default();
    required.sort();
    result["function"]["name"] = json!(group);
    result["function"]["parameters"]["properties"] = Value::Object(properties);
    result["function"]["parameters"]["required"] = json!(required);
    result
}

/// 合成：specs 里同组的合成一个，放在组里最先出现的那个的位置；pool 是别处（按需的）同组成员的定义，拿来补参数
pub fn apply(specs: Vec<Value>, pool: &[Value]) -> Vec<Value> {
    let mut result = Vec::new();
    let mut done: Vec<&str> = Vec::new();
    for spec in &specs {
        let Some((group, members)) = group_of(name_of(spec)) else {
            result.push(spec.clone());
            continue;
        };
        if done.contains(group) {
            continue;
        }
        done.push(group);
        let present: Vec<&Value> =
            members.iter().filter_map(|member| specs.iter().chain(pool).find(|candidate| name_of(candidate) == *member)).collect();
        result.push(merged(group, &present));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, properties: &[&str], required: &[&str]) -> Value {
        let properties: Map<String, Value> =
            properties.iter().map(|key| (key.to_string(), json!({ "type": "string", "description": format!("{name}.{key}") }))).collect();
        json!({ "type": "function", "function": { "name": name, "description": name, "parameters": {
            "type": "object", "properties": properties, "required": required } } })
    }

    #[test]
    fn merges_in_place_with_union_and_common_required() {
        let specs = vec![
            spec("get_current_time", &[], &[]),
            spec("list_directory", &["path"], &["path"]),
            spec("read_file", &["path"], &["path"]),
            spec("find_files", &["pattern", "path"], &["pattern"]),
            spec("search_files", &["pattern", "path", "glob"], &["pattern"]),
            spec("search_keywords", &["keywords", "path", "match_any"], &["keywords"]),
            spec("delegate_task", &["task", "name"], &["task"]),
        ];
        let pool = vec![spec("run_parallel_tasks", &["tasks", "deadline_seconds"], &["tasks"])];
        let merged = apply(specs, &pool);
        let names: Vec<&str> = merged.iter().map(name_of).collect();
        assert_eq!(names, vec!["get_current_time", "find_files", "read_file", "search_files", "delegate_task"]);
        // find_files 打底（主成员），放在 list_directory 原来的位置
        assert_eq!(merged[1]["function"]["description"], "find_files");
        assert_eq!(merged[1]["function"]["parameters"]["required"], json!([]));
        let search = merged[3]["function"]["parameters"]["properties"].as_object().unwrap();
        assert_eq!(search.keys().cloned().collect::<Vec<_>>().len(), 5);
        assert_eq!(search["path"]["description"], "search_files.path");
        // 按需里的并联补进来
        assert!(merged[4]["function"]["parameters"]["properties"]["tasks"].is_object());
        assert_eq!(merged[4]["function"]["parameters"]["required"], json!([]));
    }

    #[test]
    fn picks_member_by_arguments() {
        let args = |value: Value| value.as_object().unwrap().clone();
        assert_eq!(internal("search_files", &args(json!({ "keywords": "a b" }))), "search_keywords");
        assert_eq!(internal("search_files", &args(json!({ "pattern": "a|b", "keywords": "x" }))), "search_files");
        assert_eq!(internal("find_files", &args(json!({ "path": "src" }))), "list_directory");
        assert_eq!(internal("find_files", &args(json!({ "pattern": "*.rs" }))), "find_files");
        assert_eq!(internal("delegate_task", &args(json!({ "tasks": [{ "task": "x" }] }))), "run_parallel_tasks");
        assert_eq!(internal("delegate_task", &args(json!({ "task": "x" }))), "delegate_task");
        assert_eq!(internal("search_keywords", &args(json!({ "keywords": "a" }))), "search_keywords");
        assert!(covers("search_files", "search_keywords") && covers("read_file", "read_file") && !covers("read_file", "write_file"));
        let given = vec!["delegate_task".to_string()];
        assert!(absorbed("run_parallel_tasks", &given) && !absorbed("run_workflow", &given) && !absorbed("delegate_task", &given));
    }
}
