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
use crate::vision::{self, sees};

/// 一轮里最多请求几次模型（到了让模型把目前的结果交出来）
const MAX_TOOL_ROUNDS: i64 = 200;
/// 一段请求出错（断网、繁忙、限流）最多重试几次
const MAX_ATTEMPTS: i64 = 6;
/// 被限流（HTTP 429）累计最多等多久（秒）：同 Friday 的 OpenAIChatEngine——限流不算进重试次数，等一会儿重发，
/// 累计等了 10 分钟还不行、或服务说要等更久，当成额度用完（这一轮先停下，到点接着做）。EDITH_RATE_LIMIT_PATIENCE 只给测试用
fn rate_limit_patience() -> f64 {
    std::env::var("EDITH_RATE_LIMIT_PATIENCE").ok().and_then(|value| value.parse::<f64>().ok()).unwrap_or(600.0)
}

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
    /// 2026-11-26 额度用完 / 余额不足，在等它恢复（同 Friday 的「等额度」）：发出去还是 notice（text 给人看），数据里多带
    /// quota {kind: usage / balance, resumeAt}——看会话动静的（Friday 助手的状态板）据此标「等额度」，不当成卡住
    Quota { text: String, kind: String, resume_at: String },
    /// 别处执行的工具的活动（Friday 的 Claude Code 能力：Claude Code 自己的智能体跑的工具，流里 chunk 顶层的 x_friday.tool）：
    /// 只显示、不执行；同一个 id 先 running，再 success / failed / waiting（等用户确认，用户回「允许」就放行）
    External { id: String, name: String, input: Value, status: String, output: Option<String> },
}

/// 交给别的智能体做的 Friday 能力（power.json 的 agentChat）：Claude Code（用户自己的 Claude Code 和它的登录）；
/// 2026-11-14 去掉了 OpenCode（同 Friday）
pub const AGENT_POWERS: [&str; 1] = ["power://claudecode"];

/// 这个模型是交给别的智能体做的 Friday 能力：活交给那边自己的智能体，EDITH 的工具、追问都不给它
pub fn is_agent_power(model: &Model) -> bool {
    AGENT_POWERS.contains(&model.base_url.as_str())
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
    /// 这段会话专用的系统提示（建会话时能力给的；接在主智能体的后面，子智能体不带）
    pub instructions: String,
    /// v2.86 主智能体这一轮是第几轮（read_history 的轮次、省略步骤的说明按它）；子智能体 None
    pub history_turn: Option<i64>,
    /// v2.86 会话原样历史里调过的工具（只留助手消息的 tool_calls）：按需工具按它算——交给模型的那份较早的轮次可能折成了骨架、省略了
    pub prior_calls: Option<std::sync::Arc<Vec<Value>>>,
    /// v2.86 这一轮前面几段（交付检查追的、钩子让接着做的）已经接上、还没存档的消息（read_history 看这一轮用）
    pub pending: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
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
    /// 工作方式：workflow = 工作流（主智能体先定完整流程交给子智能体，回来验收）；default = EDITH 原来的（需要时自己派）；
    /// 没给时按 edith --workflow（DEFAULT_WORKFLOW）
    pub team: Option<String>,
    /// 2026-11-25 每次最多连着调几轮工具（1–10；Friday 助手设置的 toolRounds，助手会话用）：到了就不带工具再请求一次、交付已有的结果。
    /// 只管主智能体（派的子智能体照 EDITH 自己的上限）；没给照 MAX_TOOL_ROUNDS
    pub tool_rounds: Option<i64>,
}

/// edith --workflow：这个进程里没指明工作方式的都按工作流
static DEFAULT_WORKFLOW: AtomicBool = AtomicBool::new(false);

pub fn set_default_workflow(on: bool) {
    DEFAULT_WORKFLOW.store(on, Ordering::Relaxed);
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
            // Friday 的任务模式「工作流」原始值是 ultra
            team: text("team", &["workflow", "ultra", "default"]).map(|team| if team == "ultra" { "workflow".into() } else { team }),
            tool_rounds: params.get("toolRounds").and_then(Value::as_i64).filter(|rounds| (1..=10).contains(rounds)),
        }
    }

    /// 这一轮按工作流做
    pub fn workflow(&self) -> bool {
        match self.team.as_deref() {
            Some(team) => team == "workflow",
            None => DEFAULT_WORKFLOW.load(Ordering::Relaxed),
        }
    }

    pub fn plan(&self) -> bool {
        self.mode.as_deref() == Some("plan")
    }
}

/// 一轮的结果
pub struct Turn {
    /// 这一轮之后交给模型的历史（折叠、压缩过的；接着追一段时用它）
    pub history: Vec<Value>,
    /// v2.86 这一段接上的消息（原样：存进会话的是它）
    pub appended: Vec<Value>,
    /// 最终回答
    pub answer: String,
    pub error: Option<String>,
    pub stopped: bool,
}

/// 装提示词：自带的三份 + 数据目录里可改的那份（~/.edith/Prompts）
pub fn setup_prompts() {
    let bundled = [
        ("prompts-edith.md", include_str!("../prompts/prompts-edith.md")),
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
    prompts::setup(bundled, Some(dir.clone()), platform);
    *PROMPT_FILES.lock().unwrap() = prompt_signature(&dir);
}

/// 上次读提示词时各文件的修改时间和大小
static PROMPT_FILES: std::sync::Mutex<Vec<(String, Option<std::time::SystemTime>, u64)>> = std::sync::Mutex::new(Vec::new());

fn prompt_signature(dir: &Path) -> Vec<(String, Option<std::time::SystemTime>, u64)> {
    prompts::prompt_files(dir)
        .into_iter()
        .map(|name| {
            let info = std::fs::metadata(dir.join(&name)).ok();
            let modified = info.as_ref().and_then(|info| info.modified().ok());
            (name, modified, info.map_or(0, |info| info.len()))
        })
        .collect()
}

/// 提示词文件变了（用户、智能体改的，Friday 设置里回滚 / 恢复成自带的）：这一段请求前重新读，在跑着的大脑不用重起
pub fn refresh_prompts() {
    let Some(dir) = prompts::directory() else { return };
    let now = prompt_signature(&dir);
    let mut last = PROMPT_FILES.lock().unwrap();
    if *last != now {
        *last = now;
        prompts::reload();
    }
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
            ("capabilities", capabilities),
        ]),
        None,
    ) + "\n\n"
        + &prompts::text("edith.team", &args(&[]), None)
}

/// 系统提示后面接的：按需工具的清单、技能的清单（deferred.rs、skills.rs）
fn on_demand(context: &Context) -> String {
    // 能派子智能体的才列现成的子智能体
    let agents = if crate::team::available(context).contains(&"delegate_task") { crate::subagents::prompt() } else { String::new() };
    [crate::deferred::prompt(context), agents, crate::skills::prompt(), crate::memory::prompt(), crate::home::extra_prompt()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 给模型的工具（只给这一轮许可的）：一直给的（自带的、delegate_task、load_tools、有技能时 use_skill）+ 历史里取回过的按需工具
/// （按第一次取回的先后接在后面，前面不变）
/// 历史里调过的工具：只留助手消息的 tool_calls（按需工具按它算，见 deferred::loaded）
/// 同 Friday v2.92：主智能体调过的工具 + 存档折叠过时在第一处折叠的位置补一次 read_history（之后一直给它，工具顺序不变）
pub fn calls_with_folds(history: &[Value], folds: &Map<String, Value>) -> Vec<Value> {
    let first = folds.keys().filter_map(|key| key.parse::<usize>().ok()).min();
    let mut calls = Vec::new();
    for (index, message) in history.iter().enumerate() {
        if first == Some(index) {
            calls.push(json!({ "role": "assistant", "tool_calls": [{ "function": { "name": crate::history::NAME, "arguments": "{}" } }] }));
        }
        calls.extend(calls_of(std::slice::from_ref(message)));
    }
    calls
}

pub fn calls_of(history: &[Value]) -> Vec<Value> {
    history
        .iter()
        .filter(|message| message["role"] == "assistant" && message["tool_calls"].as_array().is_some_and(|calls| !calls.is_empty()))
        .map(|message| json!({ "role": "assistant", "tool_calls": message["tool_calls"] }))
        .collect()
}

fn spec_name(spec: &Value) -> String {
    spec["function"]["name"].as_str().unwrap_or("").to_string()
}

/// 设置里关掉的工具（config.json 的 disabledTools，Friday 的设置 › 工具）不给；现成的子智能体限定了工具：只给这些
/// （取回按需工具的 load_tools 留着；限定的列表里写合并名也认）
fn usable(context: &Context, specs: &mut Vec<Value>) {
    let disabled = crate::home::disabled_tools();
    specs.retain(|spec| !disabled.contains(&spec_name(spec)));
    if let Some(only) = context.me.as_ref().and_then(|me| me.tools.as_ref()) {
        specs.retain(|spec| {
            only.iter().any(|listed| crate::merge::covers(listed, &spec_name(spec))) || spec_name(spec) == crate::deferred::LOAD
        });
    }
}

/// 一直给的自带工具和派发工具（合并前）
fn core_specs(context: &Context) -> Vec<Value> {
    let allowed = &context.tools;
    let mut specs: Vec<Value> = tools::given_specs().into_iter().filter(|spec| allowed.allows(&spec_name(spec), false)).collect();
    specs.extend(
        crate::team::specs(context)
            .into_iter()
            .filter(|spec| crate::team::always(context).contains(&spec_name(spec).as_str()) && allowed.allows(&spec_name(spec), false)),
    );
    usable(context, &mut specs);
    specs
}

/// 一直给的自带工具和派发工具的名字（合并前）：同组的按需工具并进它们，不单列（merge::absorbed）
pub fn given_names(context: &Context) -> Vec<String> {
    core_specs(context).iter().map(spec_name).collect()
}

pub fn tool_specs(context: &Context, history: &[Value]) -> Vec<Value> {
    let allowed = &context.tools;
    let name = spec_name;
    let mut specs = core_specs(context);
    if !crate::deferred::available(context).is_empty() || !crate::mcp::unknown().is_empty() {
        specs.push(crate::deferred::spec());
    }
    if !crate::skills::list().is_empty() {
        specs.push(crate::skills::spec());
    }
    specs.extend(crate::memory::specs().into_iter().filter(|spec| allowed.allows(&name(spec), name(spec) == crate::memory::SEARCH)));
    // 能力声明了一直给的动作（Friday 助手管会话的工具，只给助手那段会话）：不经 load_tools
    specs.extend(crate::capabilities::always_specs(allowed, &context.session));
    // v2.86 看这段会话较早的原文：同 Friday v2.92 折叠过才给（按需工具里不列，calls_with_folds 在第一处折叠补一次调用）
    specs.extend(crate::deferred::specs(context, history));
    usable(context, &mut specs);
    // 同 Friday v2.90：同一组的合成一个（merge.rs）；并进来的按需成员（run_parallel_tasks）从这里补参数
    let mut pool: Vec<Value> = crate::team::specs(context).into_iter().filter(|spec| allowed.allows(&name(spec), false)).collect();
    usable(context, &mut pool);
    crate::merge::apply(specs, &pool)
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
    // 合并给模型的工具（merge.rs）换回内部的细分工具：之后权限、批准、钩子、执行都按它
    let name = &crate::merge::internal(name, arguments);
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
    if let Some(only) = context.me.as_ref().and_then(|me| me.tools.as_ref())
        && name != crate::deferred::LOAD
        && !only.iter().any(|tool| crate::merge::covers(tool, name) || *tool == canonical)
    {
        return refused(format!("你能用的工具只有：{}（{name} 不在里面）", only.join("、")));
    }
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
    // 2026-11-26 能力的动作用了光名字（提示词里写的「调用 analyze_image」，给模型的是「friday__analyze_image」；glm-4.7-flash 实测
    // 被拒收图后就这样调）：有这个动作的能力就认成它（这一轮许可的才行）
    if !name.contains(capabilities::SEPARATOR)
        && let Some(full) = capabilities::action_named(name)
        && context.tools.allows(&full, capabilities::is_read_only(&full))
        && let Some(outcome) = capabilities::call(&full, arguments, context, cancel)
    {
        return outcome;
    }
    Outcome { ok: false, text: format!("没有这个工具：{name}") }
}

enum Request {
    Done {
        pass: engine_loop::Pass,
        started: i64,
        finished: i64,
        first: Option<i64>,
        /// 这段请求带着图、接口收了
        images: bool,
    },
    Retry,
    /// 被限流（HTTP 429）：等多久、要不要转成等额度由 run 按累计等了多久定（同 Friday），不算进重试次数。reset：服务说的多久后恢复（秒）
    RateLimited {
        status: u16,
        reset: Option<f64>,
        message: String,
    },
    Failed(String),
    /// 额度用完 / 余额不足（同 Friday：等它恢复后接着做，不算进重试次数）：服务说了几点恢复就是 resume_at
    Quota {
        kind: quota::WaitKind,
        resume_at: Option<chrono::DateTime<chrono::Utc>>,
        message: String,
    },
}

/// 记下这次请求的用量（usage.rs，同 Friday 的顺手记），推 usage 事件给能力（edith-friday 转成 Friday 的 serviceUsage）
fn note_usage(model: &Model, context: &Context, observed: bool) {
    let key = crate::usage::observed_key(&model.base_url, &model.secret_name);
    crate::mind::emit(&context.session, &context.turn, "usage", json!({ "observedKey": key, "observed": observed }));
}

fn wait_kind(limit: Limit) -> Option<quota::WaitKind> {
    match limit {
        Limit::Usage => Some(quota::WaitKind::Usage),
        Limit::Balance => Some(quota::WaitKind::Balance),
        Limit::RateLimit | Limit::Denied => None,
    }
}

fn kind_name(kind: quota::WaitKind) -> &'static str {
    match kind {
        quota::WaitKind::Usage => "usage",
        quota::WaitKind::Balance => "balance",
    }
}

/// 发一段请求、读完流
/// 图片在历史里只记路径（file://…，存档不变大）：发请求时才读成 base64；模型看不了图、文件没了的换成一句话
const IMAGE_PREFIX: &str = "file://";

/// 带图的用户消息：图记路径。模型看不了图也照样记（发请求时 inline_images 换成写着路径的一句话，模型能照路径调 analyze_image；
/// 存档里留着图，换成能看图的模型时还能看）
fn user_content(text: &str, images: &[String]) -> Value {
    if images.is_empty() {
        return json!(text);
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    parts.extend(images.iter().map(|path| json!({ "type": "image_url", "image_url": { "url": format!("{IMAGE_PREFIX}{path}") } })));
    Value::Array(parts)
}

/// 2026-11-26 带着图被拒、接口说这个模型不收图（chat_request::refused 的 `vision: false`）：记成看不了（vision::refused，
/// 这个进程里 + config.json），这个循环历史里每条带图的消息末尾先写上图在哪，再由 Loop::drop_images 去掉图、接上说明
/// （vision.refused-images）——重发时不再带图，之后工具的图也不再接上，模型要看就照路径调 analyze_image
fn refuse_images(brain: &mut Loop, model: &Model) {
    vision::refused(model);
    let labeled: Vec<Value> = brain
        .history()
        .iter()
        .cloned()
        .map(|mut message| {
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                let paths: Vec<String> = parts
                    .iter()
                    .filter_map(|part| part["image_url"]["url"].as_str()?.strip_prefix(IMAGE_PREFIX).map(str::to_string))
                    .collect();
                parts.extend(paths.into_iter().map(|path| json!({ "type": "text", "text": format!("（附了一张图片：{path}）") })));
            }
            message
        })
        .collect();
    brain.set_history(labeled);
    brain.drop_images(None);
}

/// 请求体里有没有真发出去的图（inline_images 读成了 base64 的；chat 的 image_url、Responses 的 input_image）
fn carries_images(body: &Value) -> bool {
    ["messages", "input"]
        .iter()
        .flat_map(|key| body[*key].as_array().into_iter().flatten())
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .any(|part| matches!(part["type"].as_str(), Some("image_url" | "input_image")))
}

/// 2026-11-26 第一次带图用这个模型、接口收了图，这一轮做完时等后台核对的结果（同 Friday 的 checkImagesDelivered）：
/// 核对出它其实看不了（接口把图悄悄丢了，刚才说的图里的内容是猜的）——这一轮用户发的图先用看图能力描述成文字（有 analyze_image 时），
/// 历史里的图换成写着路径（和描述）的一句话、之后不再给它图，接一条 vision.dropped 让它按实际内容核对（同 Friday 的
/// resumeAfterDroppedImages）。返回要不要接着做
fn images_dropped(brain: &mut Loop, model: &Model, context: &Context, cancel: &AtomicBool, on: &mut dyn FnMut(Event)) -> bool {
    vision::settle(model, vision::SETTLE);
    if cancel.load(Ordering::Relaxed) || vision::verdict(model) != Some(false) {
        return false;
    }
    let path_of = |part: &Value| part["image_url"]["url"].as_str()?.strip_prefix(IMAGE_PREFIX).map(str::to_string);
    // 这个循环里用户发的图（跟在工具结果后面的截图不算）
    let attached: Vec<String> = brain
        .appended()
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_array())
        .filter(|parts| !parts.iter().any(|part| part["text"].as_str().is_some_and(|text| text.contains(engine_loop::IMAGE_MARKER))))
        .flat_map(|parts| parts.iter().filter_map(path_of).collect::<Vec<_>>())
        .collect();
    let mut described: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(tool) = capabilities::action_named("analyze_image").filter(|_| !attached.is_empty()) {
        on(Event::Notice("这个模型其实看不了图，正在用看图模型描述附件…".into()));
        for path in attached.iter().filter(|path| Path::new(path).is_file()) {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let mut arguments = Map::new();
            arguments.insert("image".into(), json!(path));
            arguments.insert("question".into(), json!("详细描述这张图片的内容；如果图里有文字，逐字转录。"));
            if let Some(outcome) = capabilities::call(&tool, &arguments, context, cancel).filter(|outcome| outcome.ok) {
                described.insert(path.clone(), outcome.text);
            }
        }
    }
    let relabeled: Vec<Value> = brain
        .history()
        .iter()
        .cloned()
        .map(|mut message| {
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts.iter_mut() {
                    let Some(path) = path_of(part) else { continue };
                    let text = match described.get(&path) {
                        Some(description) => format!("（附了一张图片：{path}。看图模型的描述：{}）", description.trim()),
                        None => format!("（附了一张图片：{path}）"),
                    };
                    *part = json!({ "type": "text", "text": text });
                }
            }
            message
        })
        .collect();
    brain.set_history(relabeled);
    // 图都换成文字了：只是让循环记下看不了图（之后工具的图不再接上）
    brain.drop_images(None);
    on(Event::Notice("这个模型其实看不了图（接口把图悄悄丢了），已改成先用看图能力看图、让它按实际内容核对".into()));
    brain.append(vec![json!({ "role": "user", "content": prompts::text("vision.dropped", &args(&[]), None) })]);
    true
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
fn endpoint(model: &Model, context: &Context) -> Result<(String, String, Map<String, Value>), String> {
    if model.base_url.starts_with("http://") || model.base_url.starts_with("https://") {
        return Ok((model.base_url.clone(), model.api_key.clone(), Map::new()));
    }
    let agent = context.me.as_ref().map(|me| me.id.as_str());
    let cwd = crate::home::workspace().to_string_lossy().into_owned();
    let endpoint =
        capabilities::model_endpoint(&model.base_url, &model.model, &context.session, agent, &cwd, context.prefs.mode.as_deref())?;
    let base = endpoint["baseURL"].as_str().filter(|base| !base.is_empty()).ok_or("能力回的模型地址是空的")?;
    let key = endpoint["apiKey"].as_str().map(str::to_string).unwrap_or_else(|| model.api_key.clone());
    Ok((base.trim_end_matches('/').to_string(), key, endpoint["body"].as_object().cloned().unwrap_or_default()))
}

#[allow(clippy::too_many_arguments)]
fn request(
    brain: &mut Loop,
    model: &Model,
    context: &Context,
    include_tools: bool,
    show_reasoning: bool,
    attempt: i64,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Event),
) -> Request {
    let (base, api_key, extra) = match endpoint(model, context) {
        Ok(endpoint) => endpoint,
        Err(message) => return Request::Failed(message),
    };
    let engine_id = model.engine_id();
    // OpenAI 专有的接口：模型的报错点名要它才换（chat_request::refused 记下，见下面）
    let responses = chat_request::uses_responses(&engine_id);
    let url = format!("{base}/{}", if responses { "responses" } else { "chat/completions" });
    let mut body = inline_images(brain.body(include_tools, None, false), sees(model));
    if responses {
        body = friday_core::responses::body(body, &engine_id);
    }
    if let Some(body) = body.as_object_mut() {
        body.extend(extra);
    }
    let mut parser = Parser::for_engine(show_reasoning, true, &engine_id);
    let started = now_ms();
    let mut first: Option<i64> = None;
    let mut stream_error: Option<StreamError> = None;
    let mut in_reasoning = false;
    let mut finished = false;
    let mut headers: Vec<(String, String)> = Vec::new();
    let outcome = http::post_sse(&url, &api_key, &body, &mut headers, |data| {
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
        // Claude Code 的桥：它自己的智能体跑的工具（只显示）
        for tool in feed.tools {
            let text = |key: &str| tool[key].as_str().unwrap_or("").to_string();
            on(Event::External {
                id: text("id"),
                name: text("name"),
                input: tool["input"].clone(),
                status: tool["status"].as_str().unwrap_or("running").to_string(),
                output: tool["output"].as_str().map(str::to_string),
            });
        }
        if let Some(error) = feed.error {
            stream_error = Some(error);
            return false;
        }
        // Responses 接口没有 [DONE]，结束事件就是
        finished = feed.done;
        !feed.looped && !feed.done
    });
    let retry = |message: String, delay: f64, on: &mut dyn FnMut(Event)| {
        if attempt >= MAX_ATTEMPTS {
            return Request::Failed(message);
        }
        on(Event::Notice(format!("{message}——{delay:.0} 秒后重试（第 {attempt} 次）")));
        wait(delay, cancel);
        Request::Retry
    };
    let header = |name: &str| headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone());
    let now = chrono::Utc::now();
    let resume = |seconds: Option<f64>| seconds.map(|seconds| now + chrono::Duration::milliseconds((seconds * 1000.0).round() as i64));
    match outcome {
        Ok(saw_done) => {
            match stream_error {
                Some(StreamError::RateLimit { message }) => return retry(message, chat_request::retry_delay(attempt, None), on),
                Some(StreamError::Quota { limit, reset, message }) => {
                    return match wait_kind(limit) {
                        Some(kind) => Request::Quota { kind, resume_at: resume(reset), message },
                        None => Request::Failed(format!("额度用完了：{message}")),
                    };
                }
                Some(StreamError::Failed { message }) => return Request::Failed(message),
                None => {}
            }
            let result = parser.result();
            let empty = Map::new();
            let object = result.as_object().unwrap_or(&empty);
            if !cancel.load(Ordering::Relaxed)
                && let Some((transient, message)) = chat_stream::interrupted(
                    object.get("finishReason").and_then(Value::as_str),
                    saw_done || finished || object.get("loop").is_some_and(|found| !found.is_null()),
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
            let recovered = crate::usage::note_success(&model.base_url, &model.secret_name, &header, now);
            note_usage(model, context, recovered);
            Request::Done { pass: engine_loop::pass_from(object), started, finished: now_ms(), first, images: carries_images(&body) }
        }
        Err(Failure::Network(message)) if first.is_none() => {
            retry(format!("网络出错：{message}"), chat_request::retry_delay(attempt, None), on)
        }
        Err(failure @ Failure::Network(_)) => Request::Failed(failure.message()),
        Err(Failure::Status { status, body: reply, retry_after }) => {
            let status_code = status as i64;
            if let Some((prompt_tokens, context_tokens)) = chat_request::context_overflow(status_code, &reply) {
                let decision = brain.overflow(prompt_tokens, context_tokens);
                if decision["retry"].as_bool() == Some(true) && attempt < MAX_ATTEMPTS {
                    on(Event::Notice("上下文太长，压短了重发".into()));
                    return Request::Retry;
                }
                return Request::Failed(format!("上下文超过了模型的上限（{context_tokens}）：开个新会话，或把要做的拆小"));
            }
            // 改了什么记在引擎上，重发时请求体、地址按记下的拼
            if attempt < MAX_ATTEMPTS
                && let Some(fixed) = chat_request::refused(status_code, &reply, &body, &engine_id)
            {
                if fixed["vision"] == json!(false) {
                    refuse_images(brain, model);
                }
                on(Event::Notice(match (fixed["note"].as_str(), fixed["dropped"].as_array()) {
                    // 思考关不掉 / 只认某几档、不收图（friday-core 2026-11-26）：它说改成了什么
                    (Some(note), _) => note.to_string(),
                    (None, Some(names)) => {
                        let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                        format!("模型不接受参数 {}，去掉重发", names.join("、"))
                    }
                    (None, None) => "模型要用 Responses 接口，换过去重发".into(),
                }));
                return Request::Retry;
            }
            match quota::classify(status_code, &reply) {
                Some(limit @ (Limit::Usage | Limit::Balance)) => {
                    let kind = wait_kind(limit).unwrap();
                    let message = Failure::Status { status, body: reply.clone(), retry_after }.message();
                    return Request::Quota { kind, resume_at: resume(quota::reset_delay(header, &reply, now)), message };
                }
                Some(Limit::Denied) => {
                    return Request::Failed(Failure::Status { status, body: reply, retry_after }.message());
                }
                Some(Limit::RateLimit) => {
                    let reset = quota::reset_delay(header, &reply, now);
                    return Request::RateLimited { status, reset, message: Failure::Status { status, body: reply, retry_after }.message() };
                }
                _ => {}
            }
            if status == 429 || status >= 500 {
                return retry(format!("服务繁忙（HTTP {status}）"), chat_request::retry_delay(attempt, retry_after), on);
            }
            Request::Failed(Failure::Status { status, body: reply, retry_after }.message())
        }
    }
}

/// 额度用完（或限流太久）：同 Friday，等它恢复（服务说了几点就到点后 30 秒，没说按 10、20、30 分钟、之后每小时试；余额每 30 分钟），
/// 到点接着做这一段（已经做完的步骤不重做）；停下就不等了
#[allow(clippy::too_many_arguments)]
fn wait_quota(
    model: &Model,
    context: &Context,
    kind: quota::WaitKind,
    resume_at: Option<chrono::DateTime<chrono::Utc>>,
    message: &str,
    quota_waits: i64,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Event),
) {
    let now = chrono::Utc::now();
    crate::usage::note_quota(&model.base_url, &model.secret_name, kind_name(kind), resume_at, now);
    note_usage(model, context, true);
    let planned = quota::wait_for(kind, resume_at, quota_waits, now);
    let text = quota::wait_text(kind, planned.resume_at, planned.known, now);
    on(Event::Quota {
        text: format!("{text}（{}）", message.chars().take(200).collect::<String>()),
        kind: kind_name(kind).to_string(),
        resume_at: planned.resume_at.to_rfc3339(),
    });
    let mut seconds = (planned.resume_at - now).num_milliseconds().max(0) as f64 / 1000.0;
    // 测试用：最多等这么多秒（真用时没有这个变量）
    if let Some(most) = std::env::var("EDITH_QUOTA_WAIT_MAX").ok().and_then(|value| value.parse::<f64>().ok()) {
        seconds = seconds.min(most);
    }
    wait(seconds, cancel);
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
        supports_vision: sees(model),
        // Claude Code 用它自己的工具（EDITH 的不发）
        supports_tools: !is_agent_power(model),
        thinking: model.thinking.clone(),
        extra_body: model.extra_body.clone(),
    };
    refresh_prompts();
    let base = match &context.me {
        Some(me) => crate::team::instructions(cwd, me),
        None => instructions(cwd),
    };
    let mut extra = on_demand(context);
    if context.me.is_none() && !context.instructions.trim().is_empty() {
        extra =
            [context.instructions.trim().to_string(), extra].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
    }
    // 计划模式、工作流（工作流只给主智能体：它定流程、子智能体照步骤做）
    let modes = [
        context.prefs.plan().then(|| prompts::text("edith.plan-mode", &args(&[]), None)),
        (context.me.is_none() && context.prefs.workflow()).then(|| prompts::text("edith.workflow-mode", &args(&[]), None)),
    ];
    for mode in modes.into_iter().flatten() {
        extra = [extra, mode].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
    }
    // v2.86 按需工具按原样历史里调过的算（主智能体：会话存档；子智能体：它自己的历史）+ 这一段调的
    let mut calls: Vec<Value> = match &context.prior_calls {
        Some(prior) => prior.to_vec(),
        None => calls_of(&history),
    };
    let echoes = crate::folds::echoes_reasoning(&model.base_url, &model.model);
    let mut settings = Settings {
        instructions: if extra.is_empty() { base } else { format!("{base}\n\n{extra}") },
        tools: tool_specs(context, &calls),
        temperature: model.temperature,
        enable_thinking: model.enable_thinking,
        // 请求数 = 轮数 + 1（到上限时不带工具再请求一次，交付已有的结果；同 Friday 助手）
        max_tool_rounds: context.prefs.tool_rounds.filter(|_| context.me.is_none()).map_or(MAX_TOOL_ROUNDS, |rounds| rounds + 1),
        // Claude Code：被截断时也不追一条「接着写」（桥会当成用户说的下一句）
        continues_when_truncated: !is_agent_power(model),
        has_dispatch: true,
        // v2.86 同 Friday：一次运行里越跑越长时折叠较早的步骤（预算两倍），折过的步骤也有上限
        folding: Some((crate::folds::session_budget(), echoes, crate::folds::special(context.history_turn.is_some()))),
        folded_budget: crate::folds::session_folded_budget(),
        history_turn: context.history_turn,
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
    // 主智能体：之前几轮归会话的整理管（folds.rs），这一段只折自己的步骤；子智能体整段都折
    if context.me.is_none() {
        brain.fold_from_here();
    }
    brain.append(vec![json!({ "role": "user", "content": user_content(user_text, images) })]);
    let finish = |brain: &Loop, answer: String, error: Option<String>, stopped: bool| Turn {
        history: brain.history().to_vec(),
        appended: brain.appended().to_vec(),
        answer,
        error,
        stopped,
    };
    let mut answer = String::new();
    // 2026-11-26 这一轮带图用了能不能看图还没确认的模型、接口收了图（做完时等核对的结果，images_dropped）
    let mut unverified = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return finish(&brain, answer, None, true);
        }
        let mut prepared = brain.prepare(None, None);
        if prepared["probe"].as_bool() == Some(true) {
            // 本地大模型先问上下文上限：EDITH 不问，按记下的 / 报错里的算
            prepared = brain.prepare(Some(None), None);
        }
        for key in ["fitted", "folded", "trimmed"] {
            if prepared[key].is_object() {
                on(Event::Notice("历史太长，压短了较早的部分".into()));
            }
        }
        let include_tools = prepared["includeTools"].as_bool().unwrap_or(true);
        let show_reasoning = prepared["showReasoning"].as_bool().unwrap_or(true);
        let mut attempt = 1;
        let mut quota_waits = 0;
        // 这一段被限流了几次、累计等了多久（秒）
        let (mut rate_limits, mut throttled) = (0, 0.0);
        let (pass, started, finished, first) = loop {
            match request(&mut brain, model, context, include_tools, show_reasoning, attempt, cancel, on) {
                Request::Done { pass, started, finished, first, images } => {
                    // 能不能看图还没确认、接口收了图：在后台发一张小图核对它是不是真看到了（不挡这一轮）
                    if images && vision::needs_probe(model) {
                        vision::confirm(model);
                        unverified = true;
                    }
                    break (pass, started, finished, first);
                }
                Request::Retry => attempt += 1,
                Request::RateLimited { status, reset, message } => {
                    // 同 Friday（2026-11-26 239 上实测：智谱免费模型账号级限流 1302 时，以前退避 5 次、约 1 分钟就判这一轮失败）：
                    // 不算进重试次数，按 rate_limit_delay 等一会儿重发（服务说了多久就按它）；累计等了 10 分钟还不行、或服务说要等更久，
                    // 当成额度用完，走下面等额度的路（这一轮先停下，到点接着做）
                    rate_limits += 1;
                    let jitter = i64::from(uuid::Uuid::new_v4().as_bytes()[0]) * 1000 / 256;
                    let delay = chat_request::rate_limit_delay(rate_limits, reset, jitter);
                    throttled += delay.max(1.0);
                    let patience = rate_limit_patience();
                    if throttled <= patience {
                        on(Event::Notice(format!("限流（HTTP {status}）——{delay:.0} 秒后重试（第 {rate_limits} 次）")));
                        wait(delay, cancel);
                    } else {
                        let note =
                            if reset.unwrap_or(0.0) > patience { "" } else { "持续被限流超过 10 分钟，放弃这次请求。" };
                        let now = chrono::Utc::now();
                        let resume_at = reset
                            .filter(|seconds| *seconds > 0.0)
                            .map(|seconds| now + chrono::Duration::milliseconds((seconds * 1000.0).round() as i64));
                        (rate_limits, throttled) = (0, 0.0);
                        quota_waits += 1;
                        wait_quota(model, context, quota::WaitKind::Usage, resume_at, &format!("{note}{message}"), quota_waits, cancel, on);
                        attempt = 1;
                    }
                }
                Request::Quota { kind, resume_at, message } => {
                    quota_waits += 1;
                    wait_quota(model, context, kind, resume_at, &message, quota_waits, cancel, on);
                    attempt = 1;
                }
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
            _ => {
                if std::mem::take(&mut unverified)
                    && !cancel.load(Ordering::Relaxed)
                    && images_dropped(&mut brain, model, context, cancel, on)
                {
                    continue;
                }
                return finish(&brain, answer, None, cancel.load(Ordering::Relaxed));
            }
        }
        let mut results = Vec::new();
        crate::tools::set_vision(sees(model));
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
            } else if name == crate::history::NAME && context.me.is_none() {
                // v2.86 看这段会话较早的原文：存档 + 这一轮还没存的
                crate::history::run(&arguments, context, brain.appended())
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
            parts.push(json!({ "type": "text", "text": format!("{}上面是工具给的图", friday_core::engine_loop::IMAGE_MARKER) }));
            json!({ "role": "user", "content": parts })
        });
        brain.tool_results(results, images.len(), image_message, added, None);
        // 这一步取回了工具（load_tools）、或者调了还没给的按需工具：下一段请求就给它们
        if let Some(last) = brain.appended().iter().rev().find(|message| message["tool_calls"].is_array()) {
            calls.extend(calls_of(std::slice::from_ref(last)));
        }
        let tools_next = tool_specs(context, &calls);
        if tools_next.len() != settings.tools.len() {
            settings.tools = tools_next;
            brain.configure(settings.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Prefs;
    use serde_json::json;

    #[test]
    fn tool_rounds_only_takes_one_to_ten() {
        // Friday 助手设置「每次最多调用工具几轮」（1–10）；别的值、没给都当没设（照 EDITH 自己的上限）
        assert_eq!(Prefs::from_params(&json!({ "toolRounds": 4 })).tool_rounds, Some(4));
        assert_eq!(Prefs::from_params(&json!({ "toolRounds": 1 })).tool_rounds, Some(1));
        assert_eq!(Prefs::from_params(&json!({ "toolRounds": 10 })).tool_rounds, Some(10));
        for odd in [json!(0), json!(11), json!(-3), json!("4"), json!(2.5)] {
            assert_eq!(Prefs::from_params(&json!({ "toolRounds": odd })).tool_rounds, None, "{odd}");
        }
        assert_eq!(Prefs::from_params(&json!({})).tool_rounds, None);
    }
}
