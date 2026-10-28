//! 会话怎么交给模型（同 Friday v2.86 的 ContextFolding）：会话存档里是**原样**的历史（只追加；Friday 网关显示、read_history 都看它），
//! 较早的部分怎么交给模型按标记来（会话里的 `folds` 行），每轮开头 [`fold`] 成批改标记（两次之间不变，服务商的前缀缓存一直有效），
//! [`render`] 照标记拼出这一轮交给模型的那份。一轮里越跑越长由 friday-core 的对话循环管（会话折叠 + 已折叠步骤的上限）。
//!
//! 级别（.51 上 Friday 的真实存档：202 段里 34 段下一轮超过 9.6 万，最长 47 万）：
//! 1. 摘要（fold）：没折叠的部分超过 [`budget`]，最近一轮之前的工具结果、长参数、长回复换成摘要；
//! 2. 最近一轮本身太长：按步折它较早的步骤，留约半个预算；这一轮折了的步骤超过 [`session_folded_budget`] 就整步省略（omit）；
//! 3. 骨架（level 2）：摘要的那些轮超过 [`budget`]，最早的几轮只留用户的话、每段回答最后的结论和用了哪些工具；
//! 4. 省略（level 3）：骨架超过预算的三分之一，最早的几轮整轮不交，后面第一条用户消息前面写一句说明和用户在这些轮里说过的话。
//!
//! 一轮的开头是用户交来的那条消息，存档里带着 [`TURN`]（第几轮；交给模型前去掉）。没有标记的旧会话：每条用户消息开一轮。

use serde_json::{Map, Value, json};

use friday_core::engine_loop::IMAGE_MARKER;
use friday_core::folding::{self, Special};
use friday_core::prompts;

/// 存档里一轮开头那条用户消息上记的第几轮（交给模型前去掉：所有 x_ 开头的键都只在存档里）
pub const TURN: &str = "x_edith_turn";

/// 还没折叠的部分超过多少估算 token 就折（同 Friday；测试可用 EDITH_FOLD_BUDGET 调小）
pub fn budget() -> i64 {
    std::env::var("EDITH_FOLD_BUDGET").ok().and_then(|value| value.parse().ok()).filter(|value: &i64| *value > 0).unwrap_or(folding::BUDGET)
}

/// 一次运行里会话折叠的预算（子智能体、主智能体的一轮：两倍）
pub fn session_budget() -> i64 {
    budget() * 2
}

/// 一次运行里已折叠的步骤最多留多少：超过就把最早的整步去掉（friday-core 的 folding::trim；最近一轮的整步省略也按它）
pub fn session_folded_budget() -> i64 {
    session_budget() / 4
}

/// 结果不能截的工具：派子智能体、打回的（结果是子智能体交回的结论）
pub fn special() -> Special {
    Special {
        task_state: String::new(),
        whole: ["delegate_task", "run_parallel_tasks", "rework_agent", "run_workflow"].map(str::to_string).to_vec(),
    }
}

/// 接口要求把 reasoning_content 原样带回（DeepSeek）
pub fn echoes_reasoning(base_url: &str, model: &str) -> bool {
    format!("{base_url} {model}").to_lowercase().contains("deepseek")
}

fn text(key: &str, pairs: &[(&str, String)]) -> String {
    let args: prompts::Args = pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
    prompts::text(key, &args, None)
}

/// 一次整理改了什么（交给界面的提示）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Change {
    /// 折成摘要的轮数
    pub turns: usize,
    /// 最近一轮里折成摘要的步数
    pub steps: usize,
    /// 最近一轮里整步省略的步数
    pub omitted_steps: usize,
    /// 降成骨架的轮数
    pub skeletons: usize,
    /// 整轮省略的轮数
    pub omitted: usize,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        *self == Change::default()
    }

    pub fn notice(&self) -> String {
        let mut parts = Vec::new();
        if self.turns > 0 {
            parts.push(format!("较早 {} 轮的工具输出折叠成摘要", self.turns));
        }
        if self.steps > 0 {
            parts.push(format!("最近一轮较早的 {} 步折叠成摘要", self.steps));
        }
        if self.omitted_steps > 0 {
            parts.push(format!("最近一轮更早的 {} 步省略", self.omitted_steps));
        }
        if self.skeletons > 0 {
            parts.push(format!("更早的 {} 轮只留要点", self.skeletons));
        }
        if self.omitted > 0 {
            parts.push(format!("最早的 {} 轮省略", self.omitted));
        }
        format!("对话较长，已把{}（原文可用 read_history 查）", parts.join("、"))
    }
}

fn role(entry: &Value) -> &str {
    entry.get("role").and_then(Value::as_str).unwrap_or("")
}

fn mark(marks: &Map<String, Value>, index: usize) -> Option<&Value> {
    marks.get(&index.to_string())
}

fn flag(marks: &Map<String, Value>, index: usize, key: &str) -> bool {
    mark(marks, index).and_then(|mark| mark.get(key)).and_then(Value::as_bool).unwrap_or(false)
}

fn level(marks: &Map<String, Value>, index: usize) -> i64 {
    mark(marks, index).and_then(|mark| mark.get("level")).and_then(Value::as_i64).unwrap_or(0)
}

fn set(marks: &mut Map<String, Value>, index: usize, key: &str, value: Value) {
    let entry = marks.entry(index.to_string()).or_insert_with(|| json!({}));
    if let Value::Object(object) = entry {
        object.insert(key.to_string(), value);
    }
}

/// 用户消息的文字（多模态取文字部分）
pub fn text_of(entry: &Value) -> String {
    match entry.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}

/// 去掉只在存档里的键（x_ 开头）
fn stripped(entry: &Value) -> Value {
    let mut entry = entry.clone();
    if let Value::Object(object) = &mut entry {
        object.retain(|key, _| !key.starts_with("x_"));
    }
    entry
}

// MARK: - 轮次

/// 一轮：第几轮、从哪条起、到哪条为止（不含）；第一条标记之前的是第 0 轮（不分级）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    pub number: i64,
    pub start: usize,
    pub end: usize,
}

/// 按标记分轮；一个标记都没有的旧会话每条用户消息开一轮
pub fn groups(history: &[Value]) -> Vec<Group> {
    let marked = history.iter().any(|entry| entry.get(TURN).is_some());
    let mut starts: Vec<(usize, i64)> = Vec::new();
    for (index, entry) in history.iter().enumerate() {
        if marked {
            if let Some(number) = entry.get(TURN).and_then(Value::as_i64) {
                starts.push((index, number));
            }
        } else if role(entry) == "user" {
            starts.push((index, starts.len() as i64 + 1));
        }
    }
    let mut groups = Vec::new();
    if starts.first().is_none_or(|(start, _)| *start > 0) && !history.is_empty() {
        groups.push(Group { number: 0, start: 0, end: starts.first().map_or(history.len(), |(start, _)| *start) });
    }
    for (n, (start, number)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(history.len(), |(next, _)| *next);
        groups.push(Group { number: *number, start: *start, end });
    }
    groups
}

/// 下一轮是第几轮
pub fn next_turn(history: &[Value]) -> i64 {
    groups(history).iter().map(|group| group.number).max().unwrap_or(0) + 1
}

// MARK: - 交给模型的样子

/// 一条的样子（不看级别）：整步省略的没有；折了的换摘要（工具结果、长参数、长回复；思考去掉，要原样带回的留着）；其余原样
fn view(history: &[Value], marks: &Map<String, Value>, index: usize, echoes: bool, special: &Special) -> Option<Value> {
    if flag(marks, index, "omit") {
        return None;
    }
    let mut entry = stripped(&history[index]);
    if !flag(marks, index, "fold") {
        return Some(entry);
    }
    match role(&entry).to_string().as_str() {
        "tool" => {
            if let Some(content) = entry.get("content").and_then(Value::as_str) {
                let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
                entry["content"] = json!(folding::tool_result(name, content, special));
            }
        }
        "assistant" => {
            if let Some(content) = entry.get("content").and_then(Value::as_str) {
                entry["content"] = json!(folding::text(content));
            }
            if !echoes && let Value::Object(object) = &mut entry {
                object.remove("reasoning_content");
            }
            if let Some(Value::Array(calls)) = entry.get_mut("tool_calls") {
                for call in calls.iter_mut() {
                    if let Some(Value::Object(function)) = call.get_mut("function")
                        && let Some(Value::String(arguments)) = function.get("arguments")
                    {
                        let folded = folding::arguments_json(arguments);
                        function.insert("arguments".into(), json!(folded));
                    }
                }
            }
        }
        _ => {}
    }
    Some(entry)
}

fn clip_middle(text: &str, limit: usize, head: usize, tail: usize, note: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}\n…（{note}，原 {} 字）…\n{end}", chars.len())
}

/// 骨架里的用户消息：太长留开头和结尾
fn skeleton_user(text: &str, turn: i64) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 2_000 {
        return text.to_string();
    }
    self::text(
        "context-fold.skeleton-user",
        &[
            ("head", chars[..1_500].iter().collect()),
            ("tail", chars[chars.len() - 300..].iter().collect()),
            ("count", chars.len().to_string()),
            ("turn", turn.to_string()),
        ],
    )
}

/// 骨架里的其余用户消息（补充、子智能体交回的、框架追的提醒）：只留开头
fn skeleton_notice(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 800 {
        return text.to_string();
    }
    self::text("context-fold.skeleton-notice", &[("head", chars[..600].iter().collect()), ("count", chars.len().to_string())])
}

/// 骨架里的一段回答（两条用户消息之间的助手消息和工具结果）：最后的结论和用了哪些工具
fn skeleton_reply(entries: &[Value], turn: i64) -> String {
    let mut last = String::new();
    let mut tail = String::new();
    let mut names: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut calls = 0;
    for entry in entries.iter().filter(|entry| role(entry) == "assistant") {
        if let Some(content) = entry.get("content").and_then(Value::as_str).map(str::trim).filter(|content| !content.is_empty()) {
            tail = if tail.is_empty() { content.to_string() } else { format!("{tail}\n\n{content}") };
            last = content.to_string();
        }
        for call in entry.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            calls += 1;
            if !counts.contains_key(&name) {
                names.push(name.clone());
            }
            *counts.entry(name).or_insert(0) += 1;
            tail.clear();
        }
    }
    let conclusion = if tail.is_empty() { last } else { tail };
    let mut text = clip_middle(&conclusion, 600, 450, 120, "较早的回答");
    if calls > 0 {
        let mut tools = names
            .iter()
            .take(8)
            .map(|name| if counts[name] > 1 { format!("{name} ×{}", counts[name]) } else { name.clone() })
            .collect::<Vec<_>>()
            .join("、");
        if names.len() > 8 {
            tools += " 等";
        }
        let line = self::text("context-fold.skeleton-tools", &[("count", calls.to_string()), ("tools", tools), ("turn", turn.to_string())]);
        text = if text.is_empty() { line } else { format!("{text}\n{line}") };
    }
    text
}

/// 整轮省略的那几轮留的说明：第几轮到第几轮，这些轮里用户说过的话（第一条和像要求的，各截到 200 字，最多 8 条）
fn omitted_note(turns: &[i64], said: &[(i64, String)]) -> String {
    let (Some(first), Some(last)) = (turns.iter().min(), turns.iter().max()) else { return String::new() };
    let range = if first == last { first.to_string() } else { format!("{first}–{last}") };
    let said: Vec<(i64, String)> =
        said.iter().map(|(turn, text)| (*turn, text.trim().to_string())).filter(|(_, text)| !text.is_empty()).collect();
    let mut picked: Vec<&(i64, String)> = said
        .iter()
        .enumerate()
        .filter(|(n, (_, text))| *n == 0 || friday_core::task_state::looks_like_requirement(text))
        .map(|(_, item)| item)
        .collect();
    if picked.len() > 8 {
        let tail = picked.split_off(picked.len() - 7);
        picked.truncate(1);
        picked.extend(tail);
    }
    let lines = if picked.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = picked
            .iter()
            .map(|(turn, text)| {
                let clipped: String = text.chars().take(200).collect();
                format!("- 第 {turn} 轮：「{}{}」", clipped, if text.chars().count() > 200 { "…" } else { "" })
            })
            .collect();
        format!("这些轮里用户说过的话（第一条和像要求的，截短）：\n{}\n", items.join("\n"))
    };
    self::text("context-fold.omitted", &[("range", range), ("said", lines)])
}

/// 第 turn 轮较早的 count 步整步省略时留的说明
fn dropped_note(count: usize, turn: i64) -> String {
    self::text("context-fold.dropped-steps", &[("count", count.to_string()), ("turn", turn.to_string())])
}

fn prepend(entry: &mut Value, note: &str) {
    match entry.get_mut("content") {
        Some(Value::Array(parts)) => parts.insert(0, json!({ "type": "text", "text": note })),
        _ => {
            let body = entry.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            entry["content"] = json!(if body.is_empty() { note.to_string() } else { format!("{note}\n\n{body}") });
        }
    }
}

/// 照标记拼出交给模型的历史（没有 x_ 键；给模型看的截图只留最近两张）
pub fn render(history: &[Value], marks: &Map<String, Value>, echoes: bool) -> Vec<Value> {
    let special = special();
    let mut out: Vec<Value> = Vec::new();
    let mut omitted: Vec<i64> = Vec::new();
    let mut said: Vec<(i64, String)> = Vec::new();
    for group in groups(history) {
        let turn_level = if group.number > 0 { level(marks, group.start) } else { 0 };
        if turn_level >= 3 {
            omitted.push(group.number);
            said.push((group.number, text_of(&history[group.start])));
            continue;
        }
        let mut note = (!omitted.is_empty()).then(|| omitted_note(&omitted, &said));
        omitted.clear();
        said.clear();
        if turn_level == 2 {
            let mut index = group.start;
            while index < group.end {
                if role(&history[index]) == "user" {
                    let text = text_of(&history[index]);
                    let mut entry = json!({ "role": "user", "content": if index == group.start { skeleton_user(&text, group.number) } else { skeleton_notice(&text) } });
                    if let Some(note) = note.take() {
                        prepend(&mut entry, &note);
                    }
                    out.push(entry);
                    index += 1;
                    continue;
                }
                let mut end = index;
                while end < group.end && role(&history[end]) != "user" {
                    end += 1;
                }
                let reply = skeleton_reply(&history[index..end], group.number);
                if !reply.is_empty() {
                    out.push(json!({ "role": "assistant", "content": reply }));
                }
                index = end;
            }
            continue;
        }
        let mut dropped = 0;
        for index in group.start..group.end {
            let Some(mut entry) = view(history, marks, index, echoes, &special) else {
                if role(&history[index]) == "assistant" {
                    dropped += 1;
                }
                continue;
            };
            if dropped > 0 {
                let text = dropped_note(dropped, group.number);
                if role(&entry) == "assistant" {
                    prepend(&mut entry, &text);
                } else {
                    // 省掉的几步后面紧接着是用户消息：说明单独一条回答（不出现连着两条用户消息）
                    out.push(json!({ "role": "assistant", "content": text }));
                }
                dropped = 0;
            }
            if role(&entry) == "user"
                && let Some(note) = note.take()
            {
                prepend(&mut entry, &note);
            }
            out.push(entry);
        }
        if dropped > 0 {
            out.push(json!({ "role": "assistant", "content": dropped_note(dropped, group.number) }));
        }
    }
    // 给模型看的截图只留最近两张（同 friday-core 的对话循环）
    let mut kept = 0;
    for entry in out.iter_mut().rev() {
        let screenshot = role(entry) == "user"
            && entry.get("content").and_then(Value::as_array).is_some_and(|parts| {
                parts.iter().any(|part| part.get("text").and_then(Value::as_str).is_some_and(|text| text.starts_with(IMAGE_MARKER)))
            });
        if !screenshot {
            continue;
        }
        kept += 1;
        if kept > 2 {
            *entry = json!({ "role": "user", "content": format!("{IMAGE_MARKER}（较早的截图，已省略）") });
        }
    }
    out
}

// MARK: - 每轮开头整理

fn tokens(entries: &[Value]) -> i64 {
    folding::estimated_tokens(entries)
}

/// 一段（start..end）按标记交给模型的估算 token
fn rendered_tokens(history: &[Value], marks: &Map<String, Value>, indices: impl Iterator<Item = usize>, echoes: bool) -> i64 {
    let special = special();
    indices.filter_map(|index| view(history, marks, index, echoes, &special)).map(|entry| tokens(std::slice::from_ref(&entry))).sum()
}

/// 一步：一条助手消息连同后面的工具结果（start..end）
fn steps(history: &[Value], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut steps = Vec::new();
    let mut index = from;
    while index < to {
        if role(&history[index]) != "assistant" {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < to && role(&history[end]) == "tool" {
            end += 1;
        }
        steps.push((index, end));
        index = end;
    }
    steps
}

/// 发这一轮之前整理较早的上下文（成批改标记；同 Friday 的 foldHistoryIfNeeded）：
/// 1. 没折叠的部分超过预算：最近一轮之前的全部折成摘要；
/// 2. 剩下的（最近一轮）还超预算：从后往前留约半个预算的步骤，更早的折成摘要（最新一步不动）；这一轮折了的步骤超过上限整步省略；
/// 3. 摘要的那些轮超过上限：最早的降成骨架；4. 骨架超过上限：最早的整轮省略。都降到一半以下。返回改了什么
pub fn fold(history: &[Value], marks: &mut Map<String, Value>, echoes: bool) -> Change {
    fold_with(history, marks, echoes, budget())
}

/// 同 [`fold`]，预算由调用方给（摘要上限 = 预算、骨架上限 = 预算的三分之一、这一轮折了的步骤上限 = 预算的一半）
pub fn fold_with(history: &[Value], marks: &mut Map<String, Value>, echoes: bool, budget: i64) -> Change {
    let mut change = Change::default();
    let groups = groups(history);
    let Some(last) = groups.last().copied() else { return change };
    let folded_or_omitted = |marks: &Map<String, Value>, index: usize| flag(marks, index, "fold") || flag(marks, index, "omit");
    let in_level = |marks: &Map<String, Value>, index: usize| {
        groups
            .iter()
            .find(|group| group.start <= index && index < group.end)
            .is_some_and(|group| group.number > 0 && level(marks, group.start) >= 2)
    };

    // 1. 最近一轮之前的折成摘要
    let unfolded: Vec<Value> = (0..history.len())
        .filter(|index| !folded_or_omitted(marks, *index) && !in_level(marks, *index))
        .map(|index| stripped(&history[index]))
        .collect();
    if tokens(&unfolded) > budget && last.start > 0 {
        for group in groups.iter().filter(|group| group.end <= last.start) {
            let mut newly = false;
            for (index, entry) in history.iter().enumerate().take(group.end).skip(group.start) {
                if role(entry) != "user" && !folded_or_omitted(marks, index) {
                    set(marks, index, "fold", json!(true));
                    newly = true;
                }
            }
            if newly {
                change.turns += 1;
            }
        }
    }

    // 2. 最近一轮本身太长：从后往前留约半个预算，更早的步骤折成摘要
    let rest: Vec<usize> = (last.start..history.len()).filter(|index| !folded_or_omitted(marks, *index)).collect();
    if rendered_tokens(history, marks, rest.iter().copied(), echoes) > budget {
        let mut kept = 0;
        let mut newest = true;
        let all = steps(history, last.start, history.len());
        for (n, (start, _)) in all.iter().enumerate().rev() {
            if flag(marks, *start, "fold") || flag(marks, *start, "omit") {
                break;
            }
            // 这一步后面、下一步前面的用户消息也算在留着的里面
            let next = all.get(n + 1).map_or(history.len(), |(next, _)| *next);
            kept += rendered_tokens(history, marks, *start..next, echoes);
            if newest || kept <= budget / 2 {
                newest = false;
                continue;
            }
            for (earlier_start, earlier_end) in all[..=n].iter() {
                if !flag(marks, *earlier_start, "fold") && !flag(marks, *earlier_start, "omit") {
                    change.steps += 1;
                }
                for index in *earlier_start..*earlier_end {
                    if !flag(marks, index, "omit") {
                        set(marks, index, "fold", json!(true));
                    }
                }
            }
            break;
        }
    }

    // 2b. 这一轮（从用户最后一次开口算）折了的步骤太多：最早的整步省略
    let mut sizes: Vec<((usize, usize), i64)> = steps(history, last.start, history.len())
        .into_iter()
        .filter(|(start, _)| flag(marks, *start, "fold") && !flag(marks, *start, "omit"))
        .map(|(start, end)| ((start, end), rendered_tokens(history, marks, start..end, echoes)))
        .collect();
    let mut total: i64 = sizes.iter().map(|(_, size)| size).sum();
    if total > (budget * 2 / 4) {
        for ((start, end), size) in sizes.drain(..) {
            if total <= (budget * 2 / 4) / 2 {
                break;
            }
            for index in start..end {
                set(marks, index, "omit", json!(true));
            }
            total -= size;
            change.omitted_steps += 1;
        }
    }

    // 3、4. 较早的轮次（整轮都折了的）：摘要超过上限降骨架，骨架超过上限整轮省略
    let whole = |marks: &Map<String, Value>, group: &Group| {
        group.number > 0
            && group.end <= last.start
            && (group.start..group.end).all(|index| role(&history[index]) == "user" || folded_or_omitted(marks, index))
    };
    for (current, limit) in [(0, budget), (2, budget / 3)] {
        let mut candidates: Vec<(Group, i64)> = Vec::new();
        for group in groups.iter().filter(|group| whole(marks, group) && level(marks, group.start) == current) {
            let size = if current == 0 {
                rendered_tokens(history, marks, group.start..group.end, echoes)
            } else {
                tokens(&render(
                    &history[group.start..group.end],
                    &{
                        // 单独一轮按骨架算：把它当成第 1 轮拼一遍
                        let mut single = Map::new();
                        single.insert("0".into(), json!({ "level": 2 }));
                        single
                    },
                    echoes,
                ))
            };
            candidates.push((*group, size));
        }
        let mut total: i64 = candidates.iter().map(|(_, size)| size).sum();
        if total <= limit {
            continue;
        }
        for (group, size) in candidates {
            if total <= limit / 2 {
                break;
            }
            set(marks, group.start, "level", json!(if current == 0 { 2 } else { 3 }));
            total -= size;
            if current == 0 {
                change.skeletons += 1;
            } else {
                change.omitted += 1;
            }
        }
    }
    change
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() {
        // 提示词会装进数据目录：别写进真的 ~/.edith
        unsafe { std::env::set_var("EDITH_HOME", std::env::temp_dir().join(format!("edith-unit-{}", std::process::id()))) };
        crate::brain::setup_prompts();
    }

    /// 12 轮：每轮 4 步读文件（最后一轮 12 步）、结果 3000 字，前 11 轮最后写一段结论
    fn sessions() -> Vec<Value> {
        let mut history = Vec::new();
        for turn in 1..=12 {
            let ask = if turn == 1 {
                "第 1 轮：请把 a1 做好，不要改 b 目录".to_string()
            } else {
                format!("第 {turn} 轮：请把 a{turn} 做好")
            };
            history.push(json!({ "role": "user", "content": ask, TURN: turn }));
            for step in 0..(if turn == 12 { 12 } else { 4 }) {
                let id = format!("c{turn}-{step}");
                history.push(json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": id, "type": "function",
                    "function": { "name": "read_file", "arguments": format!(r#"{{"path": "a{turn}-{step}.txt"}}"#) } }] }));
                history.push(json!({ "role": "tool", "tool_call_id": id, "name": "read_file", "content": "结果".repeat(1_500) }));
            }
            if turn < 12 {
                history.push(json!({ "role": "assistant", "content": format!("第 {turn} 轮{}", "做完了。".repeat(120)) }));
            }
        }
        history
    }

    fn valid(messages: &[Value]) {
        for (n, message) in messages.iter().enumerate() {
            assert!(message.as_object().unwrap().keys().all(|key| !key.starts_with("x_")), "交给模型的没有存档专用的键：{message}");
            if n == 0 {
                continue;
            }
            let previous = &messages[n - 1];
            assert!(!(message["role"] == "user" && previous["role"] == "user"), "连着两条用户消息：{n}");
            if message["role"] == "tool" {
                assert!(previous["role"] == "tool" || previous["tool_calls"].is_array(), "工具结果跟在调用后面：{n}");
            }
        }
        // 每个调用都有结果
        let calls: Vec<&str> = messages
            .iter()
            .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
            .filter_map(|call| call["id"].as_str())
            .collect();
        let results: Vec<&str> =
            messages.iter().filter(|message| message["role"] == "tool").filter_map(|message| message["tool_call_id"].as_str()).collect();
        assert_eq!(calls, results);
    }

    /// == Friday SelfTests.foldLevels：摘要超上限降成骨架、骨架超上限整轮省略，最近一轮太长按步折、折了的步骤太多整步省略；
    /// 拼出来的说明、轮次、合法的顺序；再整理一次不动；存档那份原样
    #[test]
    fn levels() {
        setup();
        let history = sessions();
        let original = history.clone();
        let mut marks = Map::new();
        let change = fold_with(&history, &mut marks, false, 3_000);
        assert!(
            change.turns == 11 && change.skeletons > 0 && change.omitted > 0 && change.steps > 0 && change.omitted_steps > 0,
            "{change:?}"
        );
        assert!(change.notice().contains("只留要点") && change.notice().contains("read_history"));
        assert_eq!(history, original, "存档那份原样");
        let groups = groups(&history);
        let levels: Vec<i64> = groups.iter().take(11).map(|group| level(&marks, group.start)).collect();
        assert_eq!(levels.first(), Some(&3));
        assert!(levels.windows(2).all(|pair| pair[0] >= pair[1]), "越早级别越高：{levels:?}");
        assert_eq!(level(&marks, groups[11].start), 0, "最近一轮不分级");

        let rendered = render(&history, &marks, false);
        valid(&rendered);
        let first = rendered[0]["content"].as_str().unwrap();
        assert!(
            first.starts_with("[较早的对话已省略")
                && first.contains("第 1–")
                && first.contains("第 1 轮：「第 1 轮：请把 a1 做好，不要改 b 目录」"),
            "{first}"
        );
        assert!(rendered.iter().any(|message| {
            message["role"] == "assistant"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("调用了 4 次工具：read_file ×4") && text.contains("read_history 看第"))
        }));
        assert!(rendered.iter().any(|message| message["content"].as_str().is_some_and(|text| text.starts_with("（第 12 轮较早的 "))));
        assert_eq!(rendered.last().unwrap()["content"].as_str().unwrap().chars().count(), 3_000, "最新的一步原样");
        assert!(folding::estimated_tokens(&rendered) < 3_000 * 3, "压到几个预算以内：{}", folding::estimated_tokens(&rendered));

        let mut again = marks.clone();
        assert!(fold_with(&history, &mut again, false, 3_000).is_empty(), "再整理一次不动（前缀缓存不失效）");
        assert_eq!(render(&history, &again, false), rendered, "两次拼出来逐字一致");
        assert_eq!(next_turn(&history), 13);
    }

    /// 没超预算不动；没有标记的旧会话每条用户消息开一轮；省掉的几步后面紧接着用户消息时说明单独一条回答
    #[test]
    fn small_and_legacy() {
        setup();
        let short = vec![json!({ "role": "user", "content": "你好", TURN: 1 }), json!({ "role": "assistant", "content": "好" })];
        let mut marks = Map::new();
        assert!(fold_with(&short, &mut marks, false, 3_000).is_empty());
        assert_eq!(
            render(&short, &marks, false),
            vec![json!({ "role": "user", "content": "你好" }), json!({ "role": "assistant", "content": "好" })]
        );
        let legacy = vec![
            json!({ "role": "user", "content": "一" }),
            json!({ "role": "assistant", "content": "A" }),
            json!({ "role": "user", "content": "二" }),
        ];
        assert_eq!(
            groups(&legacy).iter().map(|group| (group.number, group.start, group.end)).collect::<Vec<_>>(),
            vec![(1, 0, 2), (2, 2, 3)]
        );
        // 省掉的一步后面是工作中补充（用户消息）
        let mut history = vec![json!({ "role": "user", "content": "任务", TURN: 1 })];
        history.push(json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": "a", "type": "function", "function": { "name": "run_shell", "arguments": "{}" } }] }));
        history.push(json!({ "role": "tool", "tool_call_id": "a", "name": "run_shell", "content": "出" }));
        history.push(json!({ "role": "user", "content": "也看看 b" }));
        history.push(json!({ "role": "assistant", "content": "好" }));
        let mut marks = Map::new();
        set(&mut marks, 1, "omit", json!(true));
        set(&mut marks, 2, "omit", json!(true));
        let rendered = render(&history, &marks, false);
        valid(&rendered);
        assert!(rendered[1]["content"].as_str().unwrap().starts_with("（第 1 轮较早的 1 步已省略"));
        assert_eq!(rendered[2]["content"], "也看看 b");
    }
}
