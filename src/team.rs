//! 子智能体和工作流（照 Friday 的 AgentTeamTools、SubAgentRunner、Workflow；规则在 friday-core 的 agents、delivery、liveness）：
//! - delegate_task / run_parallel_tasks：派临时子 Agent。主智能体派的在后台跑，立刻返回 id，做完结果作为一段话交回这个会话
//!   （正在干活就补充进这一轮，闲着就开始新的一轮去验收）；子 Agent 往下派的等它们做完再返回（最多 MAX_DEPTH 层）；
//! - run_workflow：主智能体定好流程（目标 + 步骤：任务、验收标准、依赖），依赖都做完的步骤同时开始（并联），写了 after 的
//!   等前面做完、带着它们的结果再开始（串联），前置没做成的不做；全部结束后各步结果连同验收标准一起交回验收；
//! - rework_agent：验收不合格打回，原来那个子 Agent 带着它的上下文接着改；同一轮打回的几个都改完一起交回；
//! - read_agent_log、message_agent、stop_agent：看运行记录、追加指示、停掉。
//!
//! 子 Agent 交回之前自己查一遍（checks.rs：没写结果、只说下一步、只交方案、写完没验证 → 原地打回），交给上级的结果后面附验收提示。
//! 同时跑几个有上限（EDITH_MAX_AGENTS，默认 4），多的排队。子 Agent 只在这个进程里活着，不存档。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use friday_core::agents;
use friday_core::prompts::{self, Args};
use serde_json::{Map, Value, json};

use crate::brain::{self, Context, Event};
use crate::checks::{self, Recorder};
use crate::home::{self, Model};
use crate::mind::{self, Origin, Urgency};
use crate::permissions::Tools;
use crate::tools::Outcome;

/// 最多几层（主智能体是第 0 层）
pub const MAX_DEPTH: usize = 3;
/// 派子智能体的工具
pub const TOOLS: &[&str] =
    &["delegate_task", "run_parallel_tasks", "run_workflow", "rework_agent", "read_agent_log", "message_agent", "stop_agent"];

/// 正在干活的是哪个子智能体（Context 里带着；主智能体是 None）
#[derive(Debug, Clone, Default)]
pub struct Me {
    pub id: String,
    pub name: String,
    pub role: String,
    pub task: String,
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Waiting,
    Running,
    Done,
    Failed,
    Stopped,
}

impl State {
    fn text(self) -> &'static str {
        match self {
            State::Waiting => "awaitingApproval",
            State::Running => "running",
            State::Done => "succeeded",
            State::Failed | State::Stopped => "failed",
        }
    }
}

struct Agent {
    id: String,
    name: String,
    role: String,
    /// 派给它的任务（自己查、验收提示按它）
    task: String,
    session: String,
    /// 上级是哪个子智能体（主智能体派的是 None）
    parent: Option<String>,
    depth: usize,
    cwd: PathBuf,
    /// 派它的那一轮的权限、来源（审批问谁看它）
    tools: Tools,
    origin: String,
    /// 通知记在哪一轮（打回时换成打回它的那一轮）
    turn: Mutex<String>,
    deadline: Option<u64>,
    state: Mutex<State>,
    /// 最后交出的话（工作流把它交给后面的步骤）
    reply: Mutex<String>,
    history: Mutex<Vec<Value>>,
    recorder: Mutex<Recorder>,
    cancel: Mutex<Arc<AtomicBool>>,
    inbox: Mutex<Vec<String>>,
}

static AGENTS: LazyLock<Mutex<HashMap<String, Arc<Agent>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// 在跑的工作流：id → (会话, 停下的标记)
type Running = HashMap<String, (String, Arc<AtomicBool>)>;
static WORKFLOWS: LazyLock<Mutex<Running>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// 会话里还有几件后台的事没交回（交回时会话已经在干活了才减：命令行等它们都交回再退出）
static PENDING: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// 一起交回的一批（同一轮打回的、一次并联派出的）：键 → (还在做的 id, 做完的结果, 交回时的开头那段提示词)
type Batch = (HashSet<String>, Vec<String>, &'static str);
static BATCHES: LazyLock<Mutex<HashMap<String, Batch>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn pending(session: &str, change: isize) {
    let mut all = PENDING.lock().unwrap();
    let count = all.entry(session.to_string()).or_default();
    *count = (*count as isize + change).max(0) as usize;
}

/// 停下这个会话派出去的全部子智能体、工作流（用户停下这一轮时）
pub fn stop_session(session: &str) {
    for agent in AGENTS.lock().unwrap().values().filter(|agent| agent.session == session) {
        agent.cancel.lock().unwrap().store(true, Ordering::Relaxed);
    }
    for (owner, flag) in WORKFLOWS.lock().unwrap().values() {
        if owner == session {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// 这个会话还有子智能体、工作流在后台没交回
pub fn busy(session: &str) -> bool {
    PENDING.lock().unwrap().get(session).is_some_and(|count| *count > 0)
}

fn text(key: &str, pairs: &[(&str, String)]) -> String {
    let args: Args = pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
    prompts::text(key, &args, None)
}

// MARK: - 同时跑几个

static SLOTS: LazyLock<(Mutex<usize>, Condvar)> = LazyLock::new(|| (Mutex::new(0), Condvar::new()));

fn limit() -> usize {
    std::env::var("EDITH_MAX_AGENTS").ok().and_then(|value| value.parse().ok()).filter(|limit| *limit > 0).unwrap_or(4)
}

/// 占一个位置（等到有空；停下了返回 None）
fn acquire(cancel: &AtomicBool) -> Option<Slot> {
    let (count, freed) = &*SLOTS;
    let mut taken = count.lock().unwrap();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if *taken < limit() {
            *taken += 1;
            return Some(Slot);
        }
        taken = freed.wait_timeout(taken, Duration::from_millis(200)).unwrap().0;
    }
}

struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        let (count, freed) = &*SLOTS;
        *count.lock().unwrap() -= 1;
        freed.notify_all();
    }
}

// MARK: - 系统提示、工具定义

/// 子智能体的系统提示
pub fn instructions(cwd: &Path, me: &Me) -> String {
    let tail = if me.depth >= MAX_DEPTH {
        text("edith.sub.solo", &[])
    } else if me.depth >= 2 {
        text("edith.sub.deep", &[("depth", me.depth.to_string())])
    } else {
        text("edith.sub.nest", &[])
    };
    let role = if me.role.trim().is_empty() { text("edith.sub.default-role", &[]) } else { me.role.clone() };
    let now = chrono::Local::now();
    text(
        "edith.sub",
        &[
            ("name", me.name.clone()),
            ("role", role),
            ("tail", tail),
            ("limit", friday_core::liveness::RESULT_LIMIT.to_string()),
            ("cwd", cwd.to_string_lossy().into_owned()),
            ("os", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)),
            ("shell", crate::tools::shell_title()),
            ("now", format!("{}（{}）", now.format("%Y-%m-%d %H:%M"), crate::tools::weekday(&now))),
        ],
    )
}

/// 这一轮能用的派发工具：主智能体全部；子智能体没到最深一层的能往下派（等结果）、打回、看记录
pub fn available(context: &Context) -> Vec<&'static str> {
    match &context.me {
        None => TOOLS.to_vec(),
        Some(me) if me.depth < MAX_DEPTH => vec!["delegate_task", "run_parallel_tasks", "rework_agent", "read_agent_log"],
        Some(_) => Vec::new(),
    }
}

fn describe(tool: &str, param: &str) -> String {
    let key = if param.is_empty() { format!("tool.{tool}") } else { format!("tool.{tool}.{param}") };
    text(&key, &[("engineHint", String::new()), ("workTail", String::new()), ("engineField", String::new())])
}

fn string(tool: &str, param: &str) -> Value {
    json!({ "type": "string", "description": describe(tool, param) })
}

fn work_enum(tool: &str, param: &str) -> Value {
    json!({ "type": "string", "enum": ["look", "change", "hard"], "description": describe(tool, param) })
}

fn thinking_enum(tool: &str, param: &str) -> Value {
    json!({ "type": "string", "enum": ["off", "low", "medium", "high"], "description": describe(tool, param) })
}

fn function(name: &str, properties: Value, required: &[&str]) -> Value {
    json!({ "type": "function", "function": {
        "name": name,
        "description": describe(name, ""),
        "parameters": { "type": "object", "properties": properties, "required": required },
    }})
}

/// 派发工具的定义（说明来自 prompts-tools.md）
pub fn specs(context: &Context) -> Vec<Value> {
    let item = |tool: &str, prefix: &str| {
        let key = |name: &str| if prefix.is_empty() { name.to_string() } else { format!("{prefix}.{name}") };
        json!({
            "task": string(tool, &key("task")),
            "name": string(tool, &key("name")),
            "role": string(tool, &key("role")),
            "work": work_enum(tool, &key("work")),
            "thinking": thinking_enum(tool, &key("thinking")),
        })
    };
    let mut delegate = item("delegate_task", "");
    delegate["deadline_seconds"] = json!({ "type": "integer", "description": describe("delegate_task", "deadline_seconds") });
    let mut step = item("run_workflow", "steps");
    step["id"] = string("run_workflow", "steps.id");
    step["check"] = string("run_workflow", "steps.check");
    step["after"] = json!({ "type": "array", "items": { "type": "string" }, "description": describe("run_workflow", "steps.after") });
    available(context)
        .into_iter()
        .map(|name| match name {
            "delegate_task" => function(name, delegate.clone(), &["task"]),
            "run_parallel_tasks" => function(
                name,
                json!({
                    "tasks": { "type": "array", "description": describe(name, "tasks"),
                               "items": { "type": "object", "properties": item(name, "tasks"), "required": ["task"] } },
                    "deadline_seconds": { "type": "integer", "description": describe(name, "deadline_seconds") },
                }),
                &["tasks"],
            ),
            "run_workflow" => function(
                name,
                json!({
                    "goal": string(name, "goal"),
                    "steps": { "type": "array", "description": describe(name, "steps"),
                               "items": { "type": "object", "properties": step.clone(), "required": ["id", "task", "check"] } },
                }),
                &["goal", "steps"],
            ),
            "rework_agent" => {
                function(name, json!({ "id": string(name, "id"), "feedback": string(name, "feedback") }), &["id", "feedback"])
            }
            "read_agent_log" => function(
                name,
                json!({ "id": string(name, "id"), "offset": { "type": "integer", "description": describe(name, "offset") } }),
                &["id"],
            ),
            "message_agent" => function(name, json!({ "id": string(name, "id"), "message": string(name, "message") }), &["id", "message"]),
            _ => function(name, json!({ "id": string(name, "id") }), &["id"]),
        })
        .collect()
}

// MARK: - 派、跑

/// 照派发的参数建一个子智能体（还没开始跑）
fn create(spec: &Value, context: &Context, cwd: &Path) -> Arc<Agent> {
    let field = |key: &str| spec.get(key).and_then(Value::as_str).filter(|value| !value.trim().is_empty());
    let task = field("task").unwrap_or("").to_string();
    let used: HashSet<String> =
        AGENTS.lock().unwrap().values().filter(|agent| agent.session == context.session).map(|agent| agent.name.clone()).collect();
    let name = agents::agent_name(field("name"), None, field("role"), &task, &used);
    let agent = Arc::new(Agent {
        id: uuid::Uuid::new_v4().simple().to_string(),
        name,
        role: field("role").unwrap_or("").to_string(),
        task,
        session: context.session.clone(),
        parent: context.me.as_ref().map(|me| me.id.clone()),
        depth: context.me.as_ref().map_or(1, |me| me.depth + 1),
        cwd: cwd.to_path_buf(),
        tools: context.tools.clone(),
        origin: context.origin.clone(),
        turn: Mutex::new(context.turn.clone()),
        deadline: spec.get("deadline_seconds").and_then(Value::as_u64).filter(|seconds| *seconds > 0),
        state: Mutex::new(State::Waiting),
        reply: Mutex::new(String::new()),
        history: Mutex::new(Vec::new()),
        recorder: Mutex::new(Recorder::default()),
        cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
        inbox: Mutex::new(Vec::new()),
    });
    AGENTS.lock().unwrap().insert(agent.id.clone(), agent.clone());
    agent
}

fn notice(agent: &Agent, text: String) {
    let turn = agent.turn.lock().unwrap().clone();
    mind::emit(&agent.session, &turn, "notice", json!({ "text": text, "agent": agent.id }));
}

/// 第一条消息：任务 + 用户的原话（子智能体看不到对话）
fn first_prompt(agent: &Agent, task: &str) -> String {
    let request = mind::request_of(&agent.session);
    let background = if request.trim().is_empty() { String::new() } else { text("edith.sub.background", &[("request", request)]) };
    text("edith.sub.task", &[("task", task.to_string()), ("background", background)])
}

/// 跑一次（第一次派、打回都走这里），等它交回；返回交给上级的话。parent_cancel：上级停下了它也停
fn work(agent: &Arc<Agent>, prompt: String, parent_cancel: Option<Arc<AtomicBool>>) -> String {
    let cancel = Arc::new(AtomicBool::new(false));
    *agent.cancel.lock().unwrap() = cancel.clone();
    let failed =
        |reason: String| text("edith.sub.failed", &[("name", agent.name.clone()), ("id", agents::short_id(&agent.id)), ("reason", reason)]);
    // 上级停下、到了时限：跟着停
    let finished = Arc::new(AtomicBool::new(false));
    {
        let (cancel, finished, agent) = (cancel.clone(), finished.clone(), agent.clone());
        let started = Instant::now();
        let deadline = agent.deadline.map(Duration::from_secs);
        std::thread::spawn(move || {
            let mut warned = false;
            while !finished.load(Ordering::Relaxed) {
                if parent_cancel.as_ref().is_some_and(|parent| parent.load(Ordering::Relaxed)) {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Some(deadline) = deadline {
                    let elapsed = started.elapsed();
                    if elapsed >= deadline && !warned {
                        agent.inbox.lock().unwrap().push(text("edith.sub.deadline", &[]));
                        warned = true;
                    }
                    if elapsed >= deadline + Duration::from_secs(60) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }
    let outcome = (|| {
        let Some(_slot) = acquire(&cancel) else { return Err((State::Stopped, "还没开始就被停掉了".to_string())) };
        *agent.state.lock().unwrap() = State::Running;
        let start = agent.recorder.lock().unwrap().mark();
        if start == 0 {
            let first = agent.task.lines().next().unwrap_or("").chars().take(60).collect::<String>();
            notice(agent, format!("子 Agent「{}」开始：{first}", agent.name));
        } else {
            notice(agent, format!("子 Agent「{}」按验收意见接着改", agent.name));
        }
        let model = Model::load().map_err(|error| (State::Failed, error))?;
        let context = Context {
            session: agent.session.clone(),
            turn: agent.turn.lock().unwrap().clone(),
            tools: agent.tools.clone(),
            origin: agent.origin.clone(),
            cancel: Some(cancel.clone()),
            me: Some(Me {
                id: agent.id.clone(),
                name: agent.name.clone(),
                role: agent.role.clone(),
                task: agent.task.clone(),
                depth: agent.depth,
            }),
        };
        let mut history = std::mem::take(&mut *agent.history.lock().unwrap());
        let mut message = prompt;
        let mut checked = HashSet::new();
        loop {
            let mut on = |event: Event| agent.recorder.lock().unwrap().on(&event);
            let mut interject = || std::mem::take(&mut *agent.inbox.lock().unwrap());
            let turn = brain::run(&model, history, &message, &agent.cwd, &cancel, &context, &mut on, &mut interject);
            history = turn.history;
            if turn.stopped {
                *agent.history.lock().unwrap() = history;
                return Err((State::Stopped, "被停掉了".to_string()));
            }
            if let Some(error) = turn.error {
                *agent.history.lock().unwrap() = history;
                return Err((State::Failed, error));
            }
            // 交回之前自己查一遍：不合格原地打回
            let followup = checks::child_followup(&agent.task, agent.recorder.lock().unwrap().since(start), &checked);
            match followup {
                Some(followup) => {
                    checked.insert(followup.kind);
                    notice(agent, format!("子 Agent「{}」{}", agent.name, followup.notice));
                    message = followup.prompt;
                }
                None => break,
            }
        }
        *agent.history.lock().unwrap() = history;
        let recorder = agent.recorder.lock().unwrap();
        let blocks = recorder.since(start);
        let reply = friday_core::delivery::final_reply(blocks).trim().to_string();
        let hint = checks::review_hint(&agent.task, &reply, blocks).unwrap_or_default();
        Ok((reply, hint))
    })();
    finished.store(true, Ordering::Relaxed);
    match outcome {
        Ok((reply, hint)) => {
            *agent.state.lock().unwrap() = State::Done;
            *agent.reply.lock().unwrap() = reply.clone();
            notice(agent, format!("子 Agent「{}」交回了结果", agent.name));
            let clipped = friday_core::liveness::clip_result(&reply, &agent.id, &home::project_dir().join("agents"));
            text(
                "edith.sub.result",
                &[("name", agent.name.clone()), ("id", agents::short_id(&agent.id)), ("result", clipped), ("hint", hint)],
            )
        }
        Err((state, reason)) => {
            *agent.state.lock().unwrap() = state;
            *agent.reply.lock().unwrap() = reason.clone();
            notice(agent, format!("子 Agent「{}」没做成：{reason}", agent.name));
            failed(reason)
        }
    }
}

/// 交回主智能体的会话：正在干活就补充进这一轮，闲着就开始新的一轮
fn deliver(session: &str, report: &str, tools: &Tools) {
    let urgency = if mind::working(session).is_some() { Urgency::Interject } else { Urgency::Queue };
    let _ = mind::submit(session, report, Origin::agent(tools.clone()), urgency, None);
}

/// 主智能体派的：在后台跑，做完交回
fn background(agent: Arc<Agent>, prompt: String, batch: Option<String>) {
    pending(&agent.session, 1);
    std::thread::spawn(move || {
        let report = work(&agent, prompt, None);
        match batch {
            Some(key) => finish_batch(&key, &agent, report),
            None => deliver(&agent.session, &report, &agent.tools),
        }
        pending(&agent.session, -1);
    });
}

/// 打回的一个改完了：同一轮打回的都改完才一起交回
fn finish_batch(key: &str, agent: &Agent, report: String) {
    let ready = {
        let mut batches = BATCHES.lock().unwrap();
        let Some((waiting, done, _)) = batches.get_mut(key) else { return deliver(&agent.session, &report, &agent.tools) };
        waiting.remove(&agent.id);
        done.push(report);
        if waiting.is_empty() { batches.remove(key).map(|(_, done, header)| (done, header)) } else { None }
    };
    if let Some((done, header)) = ready {
        let report = if done.len() == 1 {
            done.into_iter().next().unwrap()
        } else {
            text(header, &[("count", done.len().to_string()), ("results", done.join("\n\n"))])
        };
        deliver(&agent.session, &report, &agent.tools);
    }
}

// MARK: - 工具

fn refuse(message: impl Into<String>) -> Outcome {
    Outcome { ok: false, text: message.into() }
}

/// 按 id（前几位就行）找：同一个会话里、主智能体看自己派的，子智能体看自己派的
fn find(id: &str, context: &Context, any_level: bool) -> Option<Arc<Agent>> {
    let prefix = id.trim().to_lowercase();
    if prefix.chars().count() < 4 {
        return None;
    }
    let mine = context.me.as_ref().map(|me| me.id.clone());
    let all = AGENTS.lock().unwrap();
    let matches: Vec<Arc<Agent>> = all
        .values()
        .filter(|agent| agent.session == context.session && agent.id.starts_with(&prefix))
        .filter(|agent| any_level || agent.parent == mine)
        .cloned()
        .collect();
    (matches.len() == 1).then(|| matches[0].clone())
}

fn specs_of(args: &Map<String, Value>, key: &str) -> Result<Vec<Value>, String> {
    let items = args.get(key).and_then(Value::as_array).filter(|items| !items.is_empty()).ok_or(format!("{key} 要是非空数组"))?;
    let deadline = args.get("deadline_seconds").cloned();
    items
        .iter()
        .map(|item| {
            let object = item.as_object().ok_or("每一项都要是对象")?;
            let mut spec = agents::task_spec(object)?;
            if let Some(deadline) = &deadline {
                spec["deadline_seconds"] = deadline.clone();
            }
            Ok(spec)
        })
        .collect()
}

/// 派发工具（不是返回 None）
pub fn call(name: &str, args: &Map<String, Value>, cwd: &Path, context: &Context) -> Option<Outcome> {
    let cancel = context.cancel.clone();
    if !TOOLS.contains(&name) {
        return None;
    }
    if !available(context).contains(&name) {
        return Some(refuse(if context.me.is_some() {
            format!("你已经在第 {} 层了，不能用 {name}：自己把任务做完", context.me.as_ref().map_or(0, |me| me.depth))
        } else {
            format!("不能用 {name}")
        }));
    }
    Some(match name {
        "delegate_task" => {
            let spec = match agents::task_spec(args) {
                Ok(spec) => spec,
                Err(message) => return Some(refuse(message)),
            };
            if let Some(me) = &context.me
                && spec["task"].as_str().map(str::trim) == Some(me.task.trim())
            {
                return Some(refuse("派下去的必须比你自己的任务小：不能把你的整个任务原样交给一个下级，自己做"));
            }
            let agent = create(&spec, context, cwd);
            let prompt = first_prompt(&agent, &agent.task);
            if context.me.is_none() {
                background(agent.clone(), prompt, None);
                Outcome {
                    ok: true,
                    text: format!(
                        "已在后台派出子 Agent「{}」（id：{}），做完结果会自动送回来。不用等：先做别的，或者用一两句话告诉用户怎么分的工，然后结束这一轮。",
                        agent.name,
                        agents::short_id(&agent.id)
                    ),
                }
            } else {
                Outcome { ok: true, text: work(&agent, prompt, cancel) }
            }
        }
        "run_parallel_tasks" => {
            let specs = match specs_of(args, "tasks") {
                Ok(specs) => specs,
                Err(message) => return Some(refuse(message)),
            };
            let agents: Vec<Arc<Agent>> = specs.iter().map(|spec| create(spec, context, cwd)).collect();
            if context.me.is_none() {
                let lines: Vec<String> =
                    agents.iter().map(|agent| format!("- 「{}」（id：{}）", agent.name, agents::short_id(&agent.id))).collect();
                // 一起交回：一个个回来各开一轮验收，每轮都要把整段上下文再发一遍
                let key = format!("parallel:{}", uuid::Uuid::new_v4().simple());
                let ids = agents.iter().map(|agent| agent.id.clone()).collect();
                BATCHES.lock().unwrap().insert(key.clone(), (ids, Vec::new(), "edith.sub.parallel"));
                for agent in &agents {
                    background(agent.clone(), first_prompt(agent, &agent.task), Some(key.clone()));
                }
                Outcome {
                    ok: true,
                    text: format!(
                        "已在后台派出 {} 个子 Agent，全部做完后结果一起送回来：\n{}\n不用等：先做别的，或者用一两句话告诉用户怎么分的工，然后结束这一轮。",
                        agents.len(),
                        lines.join("\n")
                    ),
                }
            } else {
                // 子智能体往下派的：一起跑，都做完一起返回
                let handles: Vec<_> = agents
                    .iter()
                    .cloned()
                    .map(|agent| {
                        let cancel = cancel.clone();
                        std::thread::spawn(move || {
                            let prompt = first_prompt(&agent, &agent.task);
                            work(&agent, prompt, cancel)
                        })
                    })
                    .collect();
                let results: Vec<String> = handles.into_iter().map(|handle| handle.join().unwrap_or_default()).collect();
                Outcome { ok: true, text: results.join("\n\n") }
            }
        }
        "run_workflow" => start_workflow(args, context, cwd),
        "rework_agent" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let feedback = args.get("feedback").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let Some(agent) = find(id, context, false) else {
                return Some(refuse(format!("没有你派的子 Agent {id}（id 写结果末尾运行记录 id 的前 8 位）")));
            };
            if feedback.is_empty() {
                return Some(refuse("feedback 要写清哪里不合格、要交付什么"));
            }
            if matches!(*agent.state.lock().unwrap(), State::Waiting | State::Running) {
                return Some(refuse(format!("「{}」还在做，没交回结果：要补充要求用 message_agent", agent.name)));
            }
            *agent.turn.lock().unwrap() = context.turn.clone();
            *agent.state.lock().unwrap() = State::Waiting;
            let prompt = text("edith.sub.rework", &[("feedback", feedback)]);
            if context.me.is_none() {
                let key = format!("{}:{}", context.session, context.turn);
                BATCHES
                    .lock()
                    .unwrap()
                    .entry(key.clone())
                    .or_insert_with(|| (HashSet::new(), Vec::new(), "edith.sub.batch"))
                    .0
                    .insert(agent.id.clone());
                background(agent.clone(), prompt, Some(key));
                Outcome {
                    ok: true, text: format!("已打回「{}」，它在后台接着改；这一轮打回的都改完会一起送回来。", agent.name)
                }
            } else {
                Outcome { ok: true, text: work(&agent, prompt, cancel) }
            }
        }
        "read_agent_log" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let Some(agent) = find(id, context, true) else { return Some(refuse(format!("没有子 Agent {id}"))) };
            let state = *agent.state.lock().unwrap();
            let log = checks::log_text(&agent.name, state.text(), &agent.recorder.lock().unwrap().blocks);
            let offset = args.get("offset").and_then(Value::as_i64);
            Outcome { ok: true, text: friday_core::tool_files::page(&log, offset) }
        }
        "message_agent" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let message = args.get("message").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let Some(agent) = find(id, context, true) else { return Some(refuse(format!("没有子 Agent {id}"))) };
            if !matches!(*agent.state.lock().unwrap(), State::Waiting | State::Running) {
                return Some(refuse(format!("「{}」已经交回了：要它改就用 rework_agent", agent.name)));
            }
            agent.inbox.lock().unwrap().push(text("edith.sub.message", &[("text", message)]));
            Outcome { ok: true, text: format!("已经告诉「{}」，它做完当前这一步就会看到", agent.name) }
        }
        _ => {
            // stop_agent：子智能体，或者整个工作流
            let id = args.get("id").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
            let workflow = WORKFLOWS
                .lock()
                .unwrap()
                .iter()
                .find(|(key, (session, _))| key.starts_with(&id) && id.len() >= 4 && *session == context.session)
                .map(|(_, (_, flag))| flag.clone());
            if let Some(flag) = workflow {
                flag.store(true, Ordering::Relaxed);
                return Some(Outcome {
                    ok: true, text: "工作流停下了：在跑的步骤停掉，没开始的不再做；结束通知会送回来".into()
                });
            }
            let Some(agent) = find(&id, context, true) else { return Some(refuse(format!("没有子 Agent {id}"))) };
            agent.cancel.lock().unwrap().store(true, Ordering::Relaxed);
            Outcome { ok: true, text: format!("「{}」停下了；它的结束通知会送回来", agent.name) }
        }
    })
}

// MARK: - 工作流

/// 任务里说要用前面的结果、却没写 after 的步骤（会和前面的同时开始，拿不到前面的结果）
const UPSTREAM_MARKERS: &[&str] = &[
    "上一步",
    "前一步",
    "上一个步骤",
    "前面的步骤",
    "前面步骤",
    "上面的步骤",
    "上一步骤",
    "之前的步骤",
    "前面整理",
    "上面整理",
    "前一步骤",
    "previous step",
    "prior step",
    "earlier step",
];

fn missing_after(args: &Map<String, Value>) -> Option<String> {
    let steps = args.get("steps")?.as_array()?;
    let ids: Vec<&str> = steps.iter().filter_map(|step| step.get("id").and_then(Value::as_str)).collect();
    steps.iter().enumerate().skip(1).find_map(|(index, step)| {
        let task = step.get("task").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let after = step.get("after").filter(|after| !after.is_null() && after.as_array().is_none_or(|list| !list.is_empty()));
        let id = step.get("id").and_then(Value::as_str).unwrap_or("");
        (after.is_none() && UPSTREAM_MARKERS.iter().any(|marker| task.contains(marker))).then(|| {
            format!(
                "步骤「{id}」的任务里要用前面步骤的结果，却没写 after：没写 after 的步骤一开始就同时做，拿不到前面的结果。要先后做的写上依赖（比如 \"after\": [\"{}\"]），前面的做完、结果会自动附进它的任务；互不依赖的才不写。改好再调 run_workflow",
                ids.get(index - 1).copied().unwrap_or("前一步的 id")
            )
        })
    })
}

fn start_workflow(args: &Map<String, Value>, context: &Context, cwd: &Path) -> Outcome {
    if let Some(message) = missing_after(args) {
        return refuse(message);
    }
    let plan = match agents::workflow_plan(args) {
        Ok(plan) => plan,
        Err(message) => return refuse(message),
    };
    let goal = plan["goal"].as_str().unwrap_or("").to_string();
    let steps: Vec<Value> = plan["steps"].as_array().cloned().unwrap_or_default();
    // 没起名字的步骤用它的 id（比从任务里截一段好认）
    let agents: Vec<Arc<Agent>> = steps
        .iter()
        .map(|step| {
            let mut spec = step["spec"].clone();
            if spec.get("name").is_none() {
                spec["name"] = step["key"].clone();
            }
            create(&spec, context, cwd)
        })
        .collect();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    WORKFLOWS.lock().unwrap().insert(id.clone(), (context.session.clone(), stop.clone()));
    let started_text = agents::workflow_started(
        &id,
        &steps
            .iter()
            .zip(&agents)
            .map(|(step, agent)| json!({ "key": step["key"], "name": agent.name, "card": agent.id, "after": step["after"] }))
            .collect::<Vec<_>>(),
        None,
    );
    pending(&context.session, 1);
    let (session, tools) = (context.session.clone(), context.tools.clone());
    std::thread::spawn(move || {
        run_workflow(&id, &goal, &steps, &agents, &stop);
        WORKFLOWS.lock().unwrap().remove(&id);
        let report = text("edith.workflow.header", &[("id", agents::short_id(&id))])
            + "\n"
            + &agents::workflow_report(
                &goal,
                &steps
                    .iter()
                    .zip(&agents)
                    .map(|(step, agent)| {
                        let state = *agent.state.lock().unwrap();
                        let outcome = match state {
                            State::Done => "done",
                            State::Failed => "failed",
                            State::Stopped if agent.recorder.lock().unwrap().blocks.is_empty() => "skipped",
                            _ => "failed",
                        };
                        let result = agent.reply.lock().unwrap().clone();
                        let result = friday_core::liveness::clip_result(&result, &agent.id, &home::project_dir().join("agents"));
                        json!({ "key": step["key"], "name": agent.name, "card": agent.id, "outcome": outcome, "check": step["check"], "result": result })
                    })
                    .collect::<Vec<_>>(),
                None,
            );
        deliver(&session, &report, &tools);
        pending(&session, -1);
    });
    Outcome { ok: true, text: started_text }
}

/// 按依赖推进：能开始的同时开始，一个做完再推进一次，直到每一步都有结果
fn run_workflow(_id: &str, goal: &str, steps: &[Value], agents: &[Arc<Agent>], stop: &AtomicBool) {
    let keys: Vec<String> = steps.iter().map(|step| step["key"].as_str().unwrap_or("").to_string()).collect();
    let names: Vec<String> = agents.iter().map(|agent| agent.name.clone()).collect();
    let after: Vec<Vec<String>> = steps
        .iter()
        .map(|step| step["after"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect())
        .collect();
    let mut outcomes: Vec<Option<agents::Outcome>> = vec![None; steps.len()];
    let mut started: HashSet<usize> = HashSet::new();
    let (sender, finished) = mpsc::channel::<(usize, bool)>();
    let mut running = 0;
    loop {
        let advance = agents::workflow_advance(&keys, &names, &after, &mut outcomes, &started, stop.load(Ordering::Relaxed));
        for skipped in advance["skip"].as_array().into_iter().flatten() {
            if let Some(index) = skipped["index"].as_u64().map(|index| index as usize) {
                *agents[index].state.lock().unwrap() = State::Stopped;
                *agents[index].reply.lock().unwrap() = skipped["reason"].as_str().unwrap_or("").to_string();
            }
        }
        for index in advance["launch"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(|index| index as usize) {
            started.insert(index);
            running += 1;
            let upstream: Vec<Value> = after[index]
                .iter()
                .filter_map(|key| keys.iter().position(|other| other == key))
                .map(|other| json!({ "key": keys[other], "name": names[other], "result": agents[other].reply.lock().unwrap().clone() }))
                .collect();
            let task = agents::workflow_step_task(goal, &agents[index].task, steps[index]["check"].as_str().unwrap_or(""), &upstream, None);
            let (agent, sender) = (agents[index].clone(), sender.clone());
            std::thread::spawn(move || {
                let prompt = first_prompt(&agent, &task);
                work(&agent, prompt, None);
                let _ = sender.send((index, *agent.state.lock().unwrap() == State::Done));
            });
        }
        if advance["finished"].as_bool() == Some(true) || (running == 0 && started.len() == steps.len()) {
            break;
        }
        if running == 0 {
            // 没有在跑的、也没有能开始的（都被跳过时 finished 已经是 true）：防御
            break;
        }
        // 等一步做完；停下了就把在跑的都停掉
        loop {
            match finished.recv_timeout(Duration::from_millis(200)) {
                Ok((index, done)) => {
                    running -= 1;
                    outcomes[index] = Some(if done { agents::Outcome::Done } else { agents::Outcome::Failed });
                    break;
                }
                Err(_) if stop.load(Ordering::Relaxed) => {
                    for (index, agent) in agents.iter().enumerate() {
                        if started.contains(&index) && outcomes[index].is_none() {
                            agent.cancel.lock().unwrap().store(true, Ordering::Relaxed);
                        }
                    }
                }
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn workflow_steps_that_need_earlier_results_must_say_so() {
        let args = |after: serde_json::Value| {
            json!({ "goal": "g", "steps": [
                { "id": "read", "task": "读 notes.md", "check": "c" },
                { "id": "write", "task": "根据上一步整理出的清单写 todo.py", "check": "c", "after": after },
            ]})
            .as_object()
            .unwrap()
            .clone()
        };
        let refused = super::missing_after(&args(json!(null))).expect("该退回");
        assert!(refused.contains("「write」") && refused.contains("\"read\""), "{refused}");
        assert!(super::missing_after(&args(json!([]))).is_some());
        assert!(super::missing_after(&args(json!(["read"]))).is_none());
    }
}
