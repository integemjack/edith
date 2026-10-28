//! read_history（同 Friday v2.86 的 HistoryTool）：看这段会话较早的原文。较早的轮次交给模型时会折成摘要、只留要点或整轮省略
//! （folds.rs），一轮里折过的步骤也会整步省略（friday-core 的 folding::trim）；这些地方的说明都指到这里。
//! 原文 = 会话存档（原样，只追加）+ 这一轮还没存的（前面几段的、这一段接上的）。轮次按 folds::groups（用户交来的一条开一轮）

use serde_json::{Map, Value, json};

use crate::brain::Context;
use crate::folds::{self, Group};
use crate::sessions::Session;
use crate::tools::Outcome;

pub const NAME: &str = "read_history";

/// 一次找最多列几处
const MAX_HITS: usize = 20;

fn prompt(key: &str) -> String {
    friday_core::prompts::text(key, &Default::default(), None)
}

pub fn spec() -> Value {
    json!({ "type": "function", "function": {
        "name": NAME,
        "description": prompt("edith.tool.read_history"),
        "parameters": { "type": "object", "properties": {
            "turn": { "type": "integer", "description": prompt("edith.tool.read_history.turn") },
            "query": { "type": "string", "description": prompt("edith.tool.read_history.query") },
            "offset": { "type": "integer", "description": prompt("edith.tool.read_history.offset") },
        } },
    }})
}

/// 原文和标记：存档里的 + 这一轮还没存的（第一条补上这一轮的轮次标记）
fn original(context: &Context, appended: &[Value]) -> (Vec<Value>, Map<String, Value>) {
    let session = Session::load(&context.session);
    let (mut history, marks) = session.map(|session| (session.history, session.folds)).unwrap_or_default();
    let mut current: Vec<Value> = context.pending.as_ref().map(|pending| pending.lock().unwrap().clone()).unwrap_or_default();
    current.extend(appended.iter().cloned());
    if let (Some(first), Some(turn)) = (current.first_mut(), context.history_turn)
        && first.get(folds::TURN).is_none()
        && first["role"] == "user"
    {
        first[folds::TURN] = json!(turn);
    }
    history.extend(current);
    (history, marks)
}

pub fn run(arguments: &Map<String, Value>, context: &Context, appended: &[Value]) -> Outcome {
    let (history, marks) = original(context, appended);
    let offset = arguments.get("offset").and_then(Value::as_i64);
    let page = |text: String| Outcome { ok: true, text: friday_core::tool_files::page(&text, offset) };
    if let Some(query) = arguments.get("query").and_then(Value::as_str).map(str::trim).filter(|query| !query.is_empty()) {
        return page(search(query, &history));
    }
    if let Some(turn) = arguments.get("turn").and_then(Value::as_i64) {
        let groups = folds::groups(&history);
        let last = groups.iter().map(|group| group.number).max().unwrap_or(0);
        let Some(group) = groups.iter().find(|group| group.number == turn && turn > 0) else {
            return Outcome {
                ok: false,
                text: format!("这段会话只有第 1–{last} 轮（用户交来一条消息算一轮）。不带参数调用可以看各轮一览。"),
            };
        };
        return page(text(group, &history));
    }
    page(overview(&history, &marks))
}

fn clip(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', " ");
    let flat = flat.trim();
    if flat.chars().count() > limit { format!("{}…", flat.chars().take(limit).collect::<String>()) } else { flat.to_string() }
}

/// 这一轮现在在交给模型的那份里是什么样
fn state(group: &Group, history: &[Value], marks: &Map<String, Value>) -> &'static str {
    let mark = |index: usize, key: &str| marks.get(&index.to_string()).and_then(|mark| mark.get(key)).cloned();
    match mark(group.start, "level").and_then(|level| level.as_i64()) {
        Some(3) => return "省略",
        Some(2) => return "要点",
        _ => {}
    }
    let others: Vec<usize> = (group.start..group.end).filter(|index| history[*index]["role"] != "user").collect();
    let flagged = |index: &usize, key: &str| mark(*index, key).and_then(|value| value.as_bool()).unwrap_or(false);
    let omitted = others.iter().any(|index| flagged(index, "omit"));
    let folded = others.iter().filter(|index| flagged(index, "fold") || flagged(index, "omit")).count();
    if !others.is_empty() && folded == others.len() {
        return if omitted { "部分省略" } else { "摘要" };
    }
    if folded > 0 { "部分折叠" } else { "原样" }
}

/// 不带参数：各轮一览（第几轮、现在在上下文里是什么样、用户说了什么、做了多少事）
fn overview(history: &[Value], marks: &Map<String, Value>) -> String {
    let groups: Vec<Group> = folds::groups(history).into_iter().filter(|group| group.number > 0).collect();
    if groups.is_empty() {
        return "这段会话还没有用户交来的消息。".into();
    }
    let mut lines = vec![format!(
        "这段会话共 {} 轮（用户交来一条消息算一轮）。「现在」是这一轮在你上下文里的样子：原样、摘要（工具结果只留开头结尾）、\
         部分折叠（较早的步骤折了或省略了）、要点（只留用户的话和结论）、省略。用 turn 看某一轮的原文，query 按关键词找。",
        groups.len()
    )];
    for group in &groups {
        let entries = &history[group.start..group.end];
        let calls: usize = entries.iter().map(|entry| entry["tool_calls"].as_array().map_or(0, Vec::len)).sum();
        let mut line = format!(
            "第 {} 轮 · 现在：{} · 用户：「{}」",
            group.number,
            state(group, history, marks),
            clip(&folds::text_of(&history[group.start]), 80)
        );
        if calls > 0 {
            line += &format!("（调用 {calls} 次工具）");
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// 一轮的原文：用户的话、回答、每次工具调用的完整参数和结果
fn text(group: &Group, history: &[Value]) -> String {
    let mut lines = vec![format!("第 {} 轮的原文：", group.number)];
    let mut call = 0;
    for entry in &history[group.start..group.end] {
        match entry["role"].as_str() {
            Some("user") => lines.push(format!("【用户】{}", folds::text_of(entry))),
            Some("assistant") => {
                if let Some(content) = entry["content"].as_str().map(str::trim).filter(|content| !content.is_empty()) {
                    lines.push(format!("【回答】{content}"));
                }
                for item in entry["tool_calls"].as_array().into_iter().flatten() {
                    call += 1;
                    lines.push(format!(
                        "【调用 #{call}】{} {}",
                        item["function"]["name"].as_str().unwrap_or(""),
                        item["function"]["arguments"].as_str().unwrap_or("")
                    ));
                }
            }
            Some("tool") => lines.push(format!("【结果】{}", entry["content"].as_str().unwrap_or(""))),
            _ => {}
        }
    }
    lines.join("\n")
}

/// 按关键词找（空格分开的几个词都要出现，不分大小写）：在第几轮、哪一处、前后一段
fn search(query: &str, history: &[Value]) -> String {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let groups = folds::groups(history);
    let turn_of = |index: usize| groups.iter().find(|group| group.start <= index && index < group.end).map_or(0, |group| group.number);
    let mut hits: Vec<String> = Vec::new();
    let mut total = 0;
    let mut check = |text: &str, turn: i64, label: String| {
        let lower = text.to_lowercase();
        if !terms.iter().all(|term| lower.contains(term.as_str())) {
            return;
        }
        total += 1;
        if hits.len() >= MAX_HITS {
            return;
        }
        // 前后一段按字算（大小写换过的位置可能对不上字节：按字数找第一个词）
        let chars: Vec<char> = text.chars().collect();
        let lower_chars: Vec<char> = lower.chars().collect();
        let first: Vec<char> = terms[0].chars().collect();
        let at =
            (0..lower_chars.len().saturating_sub(first.len() - 1)).find(|start| lower_chars[*start..].starts_with(&first)).unwrap_or(0);
        let from = at.saturating_sub(80).min(chars.len());
        let to = (at + first.len() + 160).min(chars.len());
        let snippet: String = chars[from..to].iter().collect::<String>().replace('\n', " ");
        hits.push(format!(
            "第 {turn} 轮 · {label}：{}{snippet}{}",
            if from > 0 { "…" } else { "" },
            if to < chars.len() { "…" } else { "" }
        ));
    };
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (index, entry) in history.iter().enumerate() {
        let turn = turn_of(index);
        match entry["role"].as_str() {
            Some("user") => check(&folds::text_of(entry), turn, "用户".into()),
            Some("assistant") => {
                if let Some(content) = entry["content"].as_str() {
                    check(content, turn, "回答".into());
                }
                for item in entry["tool_calls"].as_array().into_iter().flatten() {
                    let name = item["function"]["name"].as_str().unwrap_or("").to_string();
                    names.insert(item["id"].as_str().unwrap_or("").to_string(), name.clone());
                    check(item["function"]["arguments"].as_str().unwrap_or(""), turn, format!("调用 {name} 的参数"));
                }
            }
            Some("tool") => {
                let name = names.get(entry["tool_call_id"].as_str().unwrap_or("")).cloned().unwrap_or_default();
                check(entry["content"].as_str().unwrap_or(""), turn, format!("{name} 的结果"));
            }
            _ => {}
        }
    }
    if total == 0 {
        return format!("这段会话里没找到「{query}」。换个说法，或者不带参数看各轮一览。");
    }
    let mut lines = vec![format!(
        "找到 {total} 处{}；要看整轮用 turn：",
        if total > hits.len() { format!("，这里列前 {} 处（多加个词缩小范围）", hits.len()) } else { String::new() }
    )];
    lines.extend(hits);
    lines.join("\n")
}
