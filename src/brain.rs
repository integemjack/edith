//! 大脑：一轮对话——用户的话 → 调模型（流式）→ 有工具调用就用工具、把结果交回模型 → 直到给出最终回答。
//! 决定都在 friday-core 的 engine_loop（截断续写、复读、工具轮次上限、上下文压缩、统计），这里只发请求、读流、用工具。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use friday_core::chat_stream::{self, Parser, StreamError};
use friday_core::engine_loop::{self, Loop, Settings};
use friday_core::prompts::{self, Platform};
use friday_core::quota::{self, Limit};
use friday_core::{chat_request, tool_aliases};
use serde_json::{Map, Value, json};

use crate::capabilities;
use crate::home::{self, Model};
use crate::http::{self, Failure};
use crate::tools::{self, Outcome};

/// 一轮里最多请求几次模型（到了让模型把目前的结果交出来）
const MAX_TOOL_ROUNDS: i64 = 200;
/// 一段请求出错（断网、繁忙、限流）最多重试几次
const MAX_ATTEMPTS: i64 = 6;

/// 干活过程中交给外面（终端、对外接口）的事
#[derive(Debug, Clone)]
pub enum Event {
    /// 回答的文字（一块块）
    Text(String),
    /// 思考（模型给了才有）
    Thinking(String),
    /// 开始用一个工具
    Tool { id: String, name: String, arguments: Value },
    /// 用完（lines：改文件、跑命令这一步加了几行、删了几行）
    Result { id: String, name: String, ok: bool, text: String, lines: Option<[usize; 2]> },
    /// 一段请求的用量（engine_loop 的统计）
    Stats(Value),
    /// 顺带说一句（重试、压了上下文……）
    Notice(String),
}

/// 这一轮在哪个会话、是哪一轮（能力的动作要知道）、能用哪些工具（话是谁交来的定的）
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub session: String,
    /// 这一轮按什么来（Friday 客户端在会话上选的）；子智能体照派它的那一轮
    pub prefs: Prefs,
    pub turn: String,
    pub tools: crate::permissions::Tools,
    /// 话是谁交来的：能力名，终端里打的是空（审批问谁看它）
    pub origin: String,
    /// 这一轮的停下标记（派出去的子智能体跟着停）
    pub cancel: Option<std::sync::Arc<AtomicBool>>,
    /// 子智能体自己是谁（主智能体是 None）
    pub me: Option<crate::team::Me>,
}

/// 这一轮按什么来（sense 带来的，只认全权的连接；没给的照 EDITH 自己的规则和 config.json）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prefs {
    /// 权限模式（同 Friday）：auto = 删除类、改系统的才问；plan = 先给计划、照 EDITH 的规则问；full = 都不问。读敏感的、访问本机 / 内网什么模式都问
    pub mode: Option<String>,
    /// 思考档位：off / low / medium / high / ultra（按 high 发）
    pub thinking: Option<String>,
    /// 用哪个模型：config.json 的 engines 里的 id
    pub engine: Option<String>,
}

impl Prefs {
    pub fn from_params(params: &Value) -> Prefs {
        let text = |key: &str, allowed: &[&str]| {
            params.get(key).and_then(Value::as_str).filter(|value| allowed.is_empty() || allowed.contains(value)).map(str::to_string)
        };
        Prefs {
            mode: text("mode", &["auto", "plan", "full"]),
            thinking: text("thinking", &["off", "low", "medium", "high", "ultra"]),
            engine: text("engine", &[]).filter(|engine| !engine.is_empty()),
        }
    }

    pub fn plan(&self) -> bool {
        self.mode.as_deref() == Some("plan")
    }
}

/// 一轮的结果
pub struct Turn {
    /// 这一轮之后的历史（存进会话）
    pub history: Vec<Value>,
    /// 最终回答
    pub answer: String,
    pub error: Option<String>,
    pub stopped: bool,
}

/// 装提示词：自带的三份 + 数据目录里可改的那份（~/.edith/Prompts）
pub fn setup_prompts() {
    let bundled = [
        ("edith.md", include_str!("../prompts/edith.md")),
        ("prompts-tools.md", include_str!("../prompts/prompts-tools.md")),
        ("prompts-framework.md", include_str!("../prompts/prompts-framework.md")),
    ]
    .into_iter()
    .map(|(name, text)| (name.to_string(), text.to_string()))
    .collect();
    let dir = home::dir().join("Prompts");
    let value = |name: &str, text: String| (name.to_string(), Platform::Value(text));
    let section = |name: &str, key: &str| (name.to_string(), Platform::Section(key.to_string()));
    let platform = vec![
        value("trash", if cfg!(target_os = "macos") { "废纸篓".into() } else { "回收站".into() }),
        value("shell", tools::shell_title()),
        if cfg!(windows) { section("shellHint", "platform.shell-hint.powershell") } else { value("shellHint", String::new()) },
        section("speakResult", "platform.speak-result.file"),
        section("toolCodeCwd", "platform.tool-code-cwd"),
        section("scriptCwd", "platform.script-cwd"),
        value("promptsDir", dir.to_string_lossy().into_owned()),
    ];
    prompts::setup(bundled, Some(dir), platform);
}

fn args(pairs: &[(&str, String)]) -> prompts::Args {
    pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect()
}

/// 系统提示
pub fn instructions(cwd: &Path) -> String {
    let list = capabilities::summary();
    let capabilities = if list.is_empty() { String::new() } else { prompts::text("edith.capabilities", &args(&[("list", list)]), None) };
    prompts::text(
        "edith.main",
        &args(&[
            ("cwd", cwd.to_string_lossy().into_owned()),
            ("os", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)),
            ("shell", tools::shell_title()),
            ("now", {
                let now = chrono::Local::now();
                format!("{}（{}）", now.format("%Y-%m-%d %H:%M"), tools::weekday(&now))
            }),
            ("capabilities", capabilities),
        ]),
        None,
    ) + "\n\n"
        + &prompts::text("edith.team", &args(&[]), None)
}

/// 系统提示后面接的：按需工具的清单、技能的清单（deferred.rs、skills.rs）
fn on_demand(context: &Context) -> String {
    [crate::deferred::prompt(context), crate::skills::prompt(), crate::memory::prompt(), crate::home::extra_prompt()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 给模型的工具（只给这一轮许可的）：一直给的（自带的、delegate_task、load_tools、有技能时 use_skill）+ 历史里取回过的按需工具
/// （按第一次取回的先后接在后面，前面不变）
pub fn tool_specs(context: &Context, history: &[Value]) -> Vec<Value> {
    let allowed = &context.tools;
    let name = |spec: &Value| spec["function"]["name"].as_str().unwrap_or("").to_string();
    let mut specs: Vec<Value> = tools::specs().into_iter().filter(|spec| allowed.allows(&name(spec), false)).collect();
    specs.extend(
        crate::team::specs(context)
            .into_iter()
            .filter(|spec| crate::team::ALWAYS.contains(&name(spec).as_str()) && allowed.allows(&name(spec), false)),
    );
    if !crate::deferred::available(context).is_empty() || !crate::mcp::unknown().is_empty() {
        specs.push(crate::deferred::spec());
    }
    if !crate::skills::list().is_empty() {
        specs.push(crate::skills::spec());
    }
    specs.extend(crate::memory::specs().into_iter().filter(|spec| allowed.allows(&name(spec), name(spec) == crate::memory::SEARCH)));
    specs.extend(crate::deferred::specs(context, history));
    // 设置里关掉的工具（config.json 的 disabledTools，Friday 的设置 › 工具）不给
    let disabled = crate::home::disabled_tools();
    specs.retain(|spec| !disabled.contains(&name(spec)));
    specs
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn wait(seconds: f64, cancel: &AtomicBool) {
    let until = std::time::Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
    while std::time::Instant::now() < until && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 钩子的输入（同 Claude Code 交给钩子命令的标准输入）
fn hook_input(context: &Context, cwd: &Path, fields: Value) -> Value {
    let mut input = json!({
        "session_id": context.session,
        "cwd": cwd.to_string_lossy(),
        "permission_mode": context.prefs.mode.clone().unwrap_or_else(|| "default".into()),
    });
    if let (Some(input), Value::Object(fields)) = (input.as_object_mut(), fields) {
        input.extend(fields);
    }
    input
}

fn hook_text(key: &str, name: &str, value: &str) -> String {
    prompts::text(key, &args(&[(name, value.to_string())]), None)
}

/// 用一个工具：调用前后有能力管的钩子（PreToolUse / PostToolUse）就跑
fn execute(call: &str, name: &str, arguments: &Map<String, Value>, cwd: &Path, cancel: &AtomicBool, context: &Context) -> Outcome {
    let mut outcome = run_tool(call, name, arguments, cwd, cancel, context);
    if outcome.ran && !cancel.load(Ordering::Relaxed) && capabilities::has_hook("PostToolUse") {
        let fields = json!({
            "tool_name": name,
            "tool_input": outcome.input.clone().map(Value::Object).unwrap_or_else(|| Value::Object(arguments.clone())),
            "tool_response": { "success": outcome.outcome.ok, "output": outcome.outcome.text },
        });
        let after = capabilities::run_hook("PostToolUse", hook_input(context, cwd, fields));
        let context_text = [after.context.join("\n"), if after.block { after.reason } else { String::new() }]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !context_text.is_empty() {
            outcome.outcome.text = format!("{}\n\n{}", outcome.outcome.text, hook_text("hooks.after", "context", &context_text));
        }
    }
    outcome.outcome
}

struct Ran {
    outcome: Outcome,
    /// 真跑了（没被权限、钩子、批准拦下）
    ran: bool,
    /// 钩子改过的参数
    input: Option<Map<String, Value>>,
}

/// 用一个工具：自带的、能力的动作；名字写走样的按 Friday 的别名表认回来
fn run_tool(call: &str, name: &str, arguments: &Map<String, Value>, cwd: &Path, cancel: &AtomicBool, context: &Context) -> Ran {
    let refused = |text: String| Ran { outcome: Outcome { ok: false, text }, ran: false, input: None };
    if crate::home::disabled_tools().iter().any(|disabled| disabled == name) {
        return refused(format!("{name} 在设置里关掉了"));
    }
    let canonical = tool_aliases::canonical_name(name);
    let read_only = capabilities::is_read_only(name) || crate::mcp::is_read_only(name) || name == crate::memory::SEARCH;
    if !context.tools.allows(name, read_only) && !context.tools.allows(&canonical, read_only) {
        return refused(format!("没有权限用 {name}：这一轮的话来自的连接{}", context.tools.describe()));
    }
    // 调用前的钩子（能力管的：Friday 的 hooks.json、能力替自己的动作说怎么批准）：拦下、要确认、免确认、改参数
    let mut verdict = crate::approval::Verdict::default();
    let mut changed: Option<Map<String, Value>> = None;
    if capabilities::has_hook("PreToolUse") {
        let fields = json!({ "tool_name": name, "tool_input": arguments });
        let before = capabilities::run_hook("PreToolUse", hook_input(context, cwd, fields));
        if before.block || before.permission.as_deref() == Some("deny") {
            return refused(hook_text("hooks.blocked", "reason", &before.reason));
        }
        verdict.need = before.need.as_ref().and_then(|need| crate::approval::need_from(name, need));
        verdict.permission = before.permission;
        verdict.reason = before.reason;
        changed = before.input;
    }
    let arguments = changed.as_ref().unwrap_or(arguments);
    let mut ran = Ran { outcome: Outcome { ok: false, text: String::new() }, ran: true, input: changed.clone() };
    // 改东西、往外发、碰敏感的：先批准（approval.rs）；名字写走样的按认回来的那个审
    let checked = if tools::BUILTINS.contains(&name) { name } else { canonical.as_str() };
    let checked_arguments = if checked == name { arguments.clone() } else { tool_aliases::canonical_arguments(checked, arguments) };
    let private_ok = match crate::approval::check(call, checked, &checked_arguments, cwd, context, cancel, &verdict) {
        Ok(private_ok) => private_ok,
        Err(text) => return refused(text),
    };
    ran.outcome = dispatch(call, name, &canonical, arguments, cwd, cancel, context, private_ok);
    ran
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    call: &str,
    name: &str,
    canonical: &str,
    arguments: &Map<String, Value>,
    cwd: &Path,
    cancel: &AtomicBool,
    context: &Context,
    private_ok: bool,
) -> Outcome {
    if name == crate::deferred::LOAD {
        return crate::deferred::load(arguments, context);
    }
    if name == crate::skills::USE {
        return crate::skills::use_skill(arguments);
    }
    if let Some(outcome) = crate::memory::run(name, arguments) {
        return outcome;
    }
    if let Some(outcome) = tools::run(name, arguments, cwd, cancel, &context.session, private_ok)
        .or_else(|| crate::team::call(name, arguments, cwd, context, call))
        .or_else(|| capabilities::call(name, arguments, context, cancel))
        .or_else(|| crate::mcp::call(name, arguments))
    {
        return outcome;
    }
    if canonical != name
        && let Some(outcome) =
            tools::run(canonical, &tool_aliases::canonical_arguments(canonical, arguments), cwd, cancel, &context.session, private_ok)
    {
        return outcome;
    }
    Outcome { ok: false, text: format!("没有这个工具：{name}") }
}

enum Request {
    Done { pass: engine_loop::Pass, started: i64, finished: i64, first: Option<i64> },
    Retry,
    Failed(String),
}

/// 发一段请求、读完流
/// 图片在历史里只记路径（file://…，存档不变大）：发请求时才读成 base64；模型看不了图、文件没了的换成一句话
const IMAGE_PREFIX: &str = "file://";

fn user_content(text: &str, images: &[String], model: &Model) -> Value {
    if images.is_empty() || !model.supports_vision {
        return json!(text);
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    parts.extend(images.iter().map(|path| json!({ "type": "image_url", "image_url": { "url": format!("{IMAGE_PREFIX}{path}") } })));
    Value::Array(parts)
}

fn image_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|extension| extension.to_str()).map(str::to_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "image/png",
    }
}

fn inline_images(mut body: Value, vision: bool) -> Value {
    use base64::Engine;
    for message in body["messages"].as_array_mut().into_iter().flatten() {
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else { continue };
        for part in parts.iter_mut() {
            let Some(path) = part["image_url"]["url"].as_str().and_then(|url| url.strip_prefix(IMAGE_PREFIX)).map(str::to_string) else {
                continue;
            };
            let bytes = std::fs::read(&path).ok().filter(|bytes| bytes.len() <= 20 * 1024 * 1024);
            *part = match bytes {
                Some(bytes) if vision => {
                    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                    json!({ "type": "image_url", "image_url": { "url": format!("data:{};base64,{data}", image_type(&path)) } })
                }
                Some(_) => json!({ "type": "text", "text": format!("（附了一张图片：{path}；这个模型看不了图）") }),
                None => json!({ "type": "text", "text": format!("（附过一张图片：{path}，已经不在了或太大）") }),
            };
        }
    }
    body
}

/// 模型的真地址：http 的照用；别的（Friday 本地模型的 power://llm）问管它的能力（model.endpoint，可能要先拉起模型服务），
/// 它回的 body 并进请求体
fn endpoint(model: &Model, session: &str) -> Result<(String, String, Map<String, Value>), String> {
    if model.base_url.starts_with("http://") || model.base_url.starts_with("https://") {
        return Ok((model.base_url.clone(), model.api_key.clone(), Map::new()));
    }
    let endpoint = capabilities::model_endpoint(&model.base_url, &model.model, session)?;
    let base = endpoint["baseURL"].as_str().filter(|base| !base.is_empty()).ok_or("能力回的模型地址是空的")?;
    let key = endpoint["apiKey"].as_str().map(str::to_string).unwrap_or_else(|| model.api_key.clone());
    Ok((base.trim_end_matches('/').to_string(), key, endpoint["body"].as_object().cloned().unwrap_or_default()))
}

#[allow(clippy::too_many_arguments)]
fn request(
    brain: &mut Loop,
    model: &Model,
    session: &str,
    include_tools: bool,
    show_reasoning: bool,
    attempt: i64,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Event),
) -> Request {
    let (base, api_key, extra) = match endpoint(model, session) {
        Ok(endpoint) => endpoint,
        Err(message) => return Request::Failed(message),
    };
    let url = format!("{base}/chat/completions");
    let mut body = inline_images(brain.body(include_tools, None), model.supports_vision);
    if let Some(body) = body.as_object_mut() {
        body.extend(extra);
    }
    let mut parser = Parser::new(show_reasoning, true);
    let started = now_ms();
    let mut first: Option<i64> = None;
    let mut stream_error: Option<StreamError> = None;
    let mut in_reasoning = false;
    let outcome = http::post_sse(&url, &api_key, &body, |data| {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { return true };
        let feed = parser.feed(&chunk, chrono::Utc::now());
        if feed.first_token {
            first = Some(now_ms());
        }
        for piece in feed.chunks {
            match piece.as_str() {
                "<think>" => in_reasoning = true,
                "</think>" => in_reasoning = false,
                _ => on(if in_reasoning { Event::Thinking(piece) } else { Event::Text(piece) }),
            }
        }
        if let Some(error) = feed.error {
            stream_error = Some(error);
            return false;
        }
        !feed.looped
    });
    let retry = |message: String, delay: f64, on: &mut dyn FnMut(Event)| {
        if attempt >= MAX_ATTEMPTS {
            return Request::Failed(message);
        }
        on(Event::Notice(format!("{message}——{delay:.0} 秒后重试（第 {attempt} 次）")));
        wait(delay, cancel);
        Request::Retry
    };
    match outcome {
        Ok(saw_done) => {
            match stream_error {
                Some(StreamError::RateLimit { message }) => return retry(message, chat_request::retry_delay(attempt, None), on),
                Some(StreamError::Quota { message, .. }) => return Request::Failed(format!("额度用完了：{message}")),
                Some(StreamError::Failed { message }) => return Request::Failed(message),
                None => {}
            }
            let result = parser.result();
            let empty = Map::new();
            let object = result.as_object().unwrap_or(&empty);
            if !cancel.load(Ordering::Relaxed)
                && let Some((transient, message)) = chat_stream::interrupted(
                    object.get("finishReason").and_then(Value::as_str),
                    saw_done || object.get("loop").is_some_and(|found| !found.is_null()),
                )
            {
                // 一个字都还没出来就断了：重发；出了一半的照收（模型下一段会接着做）
                if !transient {
                    return Request::Failed(message);
                }
                if first.is_none() {
                    return retry(message, chat_request::retry_delay(attempt, None), on);
                }
                on(Event::Notice(message));
            }
            Request::Done { pass: engine_loop::pass_from(object), started, finished: now_ms(), first }
        }
        Err(Failure::Network(message)) if first.is_none() => {
            retry(format!("网络出错：{message}"), chat_request::retry_delay(attempt, None), on)
        }
        Err(failure @ Failure::Network(_)) => Request::Failed(failure.message()),
        Err(Failure::Status { status, body, retry_after }) => {
            let status_code = status as i64;
            if let Some((prompt_tokens, context_tokens)) = chat_request::context_overflow(status_code, &body) {
                let decision = brain.overflow(prompt_tokens, context_tokens);
                if decision["retry"].as_bool() == Some(true) && attempt < MAX_ATTEMPTS {
                    on(Event::Notice("上下文太长，压短了重发".into()));
                    return Request::Retry;
                }
                return Request::Failed(format!("上下文超过了模型的上限（{context_tokens}）：开个新会话，或把要做的拆小"));
            }
            let unsupported = chat_request::unsupported_parameters(status_code, &body);
            if !unsupported.is_empty() && attempt < MAX_ATTEMPTS {
                chat_request::remember_unsupported(&model.engine_id(), &unsupported);
                on(Event::Notice(format!("模型不接受参数 {}，去掉重发", unsupported.join("、"))));
                return Request::Retry;
            }
            match quota::classify(status_code, &body) {
                Some(Limit::Usage | Limit::Balance | Limit::Denied) => {
                    return Request::Failed(Failure::Status { status, body, retry_after }.message());
                }
                Some(Limit::RateLimit) => {
                    return retry(format!("限流（HTTP {status}）"), chat_request::retry_delay(attempt, retry_after), on);
                }
                _ => {}
            }
            if status == 429 || status >= 500 {
                return retry(format!("服务繁忙（HTTP {status}）"), chat_request::retry_delay(attempt, retry_after), on);
            }
            Request::Failed(Failure::Status { status, body, retry_after }.message())
        }
    }
}

/// 干一轮。interject：工具跑完、下一段请求之前取等着补充的话（已经包好给模型看的样子）
#[allow(clippy::too_many_arguments)]
pub fn run(
    model: &Model,
    history: Vec<Value>,
    user_text: &str,
    images: &[String],
    cwd: &Path,
    cancel: &AtomicBool,
    context: &Context,
    on: &mut dyn FnMut(Event),
    interject: &mut dyn FnMut() -> Vec<String>,
) -> Turn {
    let config = engine_loop::Config {
        engine_id: model.engine_id(),
        model: model.model.clone(),
        base_url: model.base_url.clone(),
        supports_vision: model.supports_vision,
        supports_tools: true,
        thinking: model.thinking.clone(),
        extra_body: model.extra_body.clone(),
    };
    let base = match &context.me {
        Some(me) => crate::team::instructions(cwd, me),
        None => instructions(cwd),
    };
    let mut extra = on_demand(context);
    if context.prefs.plan() {
        extra = [extra, prompts::text("edith.plan-mode", &args(&[]), None)]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let mut settings = Settings {
        instructions: if extra.is_empty() { base } else { format!("{base}\n\n{extra}") },
        tools: tool_specs(context, &history),
        temperature: model.temperature,
        enable_thinking: model.enable_thinking,
        max_tool_rounds: MAX_TOOL_ROUNDS,
        continues_when_truncated: true,
        has_dispatch: true,
        ..Settings::default()
    };
    // Friday 选的思考档位（这个模型能调的才管用）
    if let Some(level) = context.prefs.thinking.as_deref().filter(|_| model.thinking != "none") {
        settings.enable_thinking = level != "off";
        settings.reasoning_effort = match level {
            "low" | "medium" | "high" => Some(level.to_string()),
            "ultra" => Some("high".into()),
            _ => None,
        };
    }
    let mut brain = Loop::new(config, history);
    brain.begin(settings.clone());
    brain.append(vec![json!({ "role": "user", "content": user_content(user_text, images, model) })]);
    let finish = |brain: &Loop, answer: String, error: Option<String>, stopped: bool| Turn {
        history: brain.history().to_vec(),
        answer,
        error,
        stopped,
    };
    let mut answer = String::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return finish(&brain, answer, None, true);
        }
        let mut prepared = brain.prepare(None, None);
        if prepared["probe"].as_bool() == Some(true) {
            // 本地大模型先问上下文上限：EDITH 不问，按记下的 / 报错里的算
            prepared = brain.prepare(Some(None), None);
        }
        for key in ["fitted", "folded"] {
            if prepared[key].is_object() {
                on(Event::Notice("历史太长，压短了较早的部分".into()));
            }
        }
        let include_tools = prepared["includeTools"].as_bool().unwrap_or(true);
        let show_reasoning = prepared["showReasoning"].as_bool().unwrap_or(true);
        let mut attempt = 1;
        let (pass, started, finished, first) = loop {
            match request(&mut brain, model, &context.session, include_tools, show_reasoning, attempt, cancel, on) {
                Request::Done { pass, started, finished, first } => break (pass, started, finished, first),
                Request::Retry => attempt += 1,
                Request::Failed(message) => {
                    if prepared["wrapUp"].as_bool() == Some(true) {
                        let text = brain.wrap_up_failed();
                        on(Event::Text(text.clone()));
                        return finish(&brain, text, None, false);
                    }
                    return finish(&brain, answer, Some(message), false);
                }
            }
            if cancel.load(Ordering::Relaxed) {
                return finish(&brain, answer, None, true);
            }
        };
        answer = pass.content.clone();
        on(Event::Stats(brain.pass_done(pass, started, finished, first, 1_000_000)));
        let decision = brain.decide(!cancel.load(Ordering::Relaxed), None);
        for chunk in decision["chunks"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            on(Event::Text(chunk.to_string()));
        }
        match decision["next"].as_str() {
            Some("prepare") => continue,
            Some("tools") => {}
            _ => return finish(&brain, answer, None, cancel.load(Ordering::Relaxed)),
        }
        let mut results = Vec::new();
        crate::tools::take_images();
        let mut images: Vec<String> = Vec::new();
        for call in decision["calls"].as_array().into_iter().flatten() {
            let id = call["id"].as_str().unwrap_or("").to_string();
            let name = call["name"].as_str().unwrap_or("").to_string();
            let arguments = call["arguments"].as_object().cloned().unwrap_or_default();
            on(Event::Tool { id: id.clone(), name: name.clone(), arguments: Value::Object(arguments.clone()) });
            crate::tools::take_lines();
            let outcome = if let Some(rejected) = call["rejected"].as_str() {
                Outcome { ok: false, text: rejected.to_string() }
            } else if cancel.load(Ordering::Relaxed) {
                Outcome { ok: false, text: "（用户停下了，没有执行）".into() }
            } else {
                execute(&id, &name, &arguments, cwd, cancel, context)
            };
            let lines = crate::tools::take_lines();
            images.extend(crate::tools::take_images());
            on(Event::Result { id: id.clone(), name: name.clone(), ok: outcome.ok, text: outcome.text.clone(), lines });
            results.push((id, name, outcome.text));
        }
        let added: Vec<Value> = interject().into_iter().map(|text| json!({ "role": "user", "content": text })).collect();
        if !added.is_empty() {
            on(Event::Notice(format!("收到 {} 段补充，接着做时一起考虑", added.len())));
        }
        // 截图：跟在工具结果后面一条用户消息（图片记路径、发请求时才读成 base64；较早的截图 friday-core 换成一句话）
        let image_message = (!images.is_empty()).then(|| {
            let mut parts: Vec<Value> =
                images.iter().map(|path| json!({ "type": "image_url", "image_url": { "url": format!("{IMAGE_PREFIX}{path}") } })).collect();
            parts.push(json!({ "type": "text", "text": format!("{}上面是工具截的图", friday_core::engine_loop::IMAGE_MARKER) }));
            json!({ "role": "user", "content": parts })
        });
        brain.tool_results(results, images.len(), image_message, added, None);
        // 这一步取回了工具（load_tools）、或者调了还没给的按需工具：下一段请求就给它们
        let tools_next = tool_specs(context, brain.history());
        if tools_next.len() != settings.tools.len() {
            settings.tools = tools_next;
            brain.configure(settings.clone());
        }
    }
}
