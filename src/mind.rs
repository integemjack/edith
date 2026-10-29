//! 调度：谁的话、什么时候交给大脑。终端里打的、能力推来的（sense / ask）都从这里进；一个会话同一时间只干一轮，
//! 正在干的时候新来的按急缓（docs/CAPABILITY-PROTOCOL.md「急缓」）：
//! - interrupt 打断：停下手上这轮，马上带着新内容重来（历史里留着停下前做了的）；
//! - interject 补充：在下一个步骤之前塞进这一轮；这一轮没有下一步了（直接答完）就紧接着为它再干一轮；
//! - queue 排队（默认）：这轮干完再干；
//! - note 只记下：记进会话，不回复。
//!
//! 干活过程中的事件（开始、文字、工具、结果、用量、提示、做完）推给所有登记的收听者（终端、能力）。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use friday_core::prompts;
use serde_json::{Value, json};

use crate::brain::{self, Event};
use crate::home::Model;
use crate::sessions::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Interrupt,
    Interject,
    Queue,
    Note,
}

impl Urgency {
    pub fn parse(text: &str) -> Option<Urgency> {
        Some(match text {
            "interrupt" => Urgency::Interrupt,
            "interject" => Urgency::Interject,
            "queue" => Urgency::Queue,
            "note" => Urgency::Note,
            _ => return None,
        })
    }
}

/// 话从哪来：终端（capability 为空）或某个能力
#[derive(Debug, Clone, Default)]
pub struct Origin {
    /// 能力名（capabilities.json 里的名字）；终端里打的是空
    pub capability: String,
    /// 能力说的是哪种感知（speech / message / vision / event …）
    pub kind: String,
    /// 能力说的是谁（比如聊天渠道里的用户名）
    pub from: String,
    /// 它交来的话那一轮能用哪些工具（令牌定的；终端里打的全部能用）
    pub tools: crate::permissions::Tools,
    /// 这一轮按什么来（权限模式、思考档位、模型；只认全权的连接）
    pub prefs: brain::Prefs,
    /// 附的图片（这台上的路径）：模型能看图时随这段话一起给它（只认全权的连接）
    pub images: Vec<String>,
}

impl Origin {
    pub fn terminal() -> Origin {
        Origin::default()
    }

    /// 子智能体、工作流交回的结果（team.rs）：权限照派它的那一轮
    pub fn agent(tools: crate::permissions::Tools) -> Origin {
        Origin { kind: "agent".into(), tools, ..Origin::default() }
    }

    pub fn to_json(&self) -> Value {
        json!({ "capability": self.capability, "kind": self.kind, "from": self.from })
    }

    /// 用户自己打的（终端里，或全权连接说是 user 的）
    fn is_user(&self) -> bool {
        self.capability.is_empty() || self.kind == "user"
    }

    fn source(&self) -> String {
        match (self.is_user(), self.kind.is_empty()) {
            (true, _) => "用户".into(),
            (false, true) => format!("能力「{}」", self.capability),
            (false, false) => format!("能力「{}」（{}）", self.capability, self.kind),
        }
    }

    /// 交给模型的那段话（终端里打的原样；子智能体交回的自带说明，也原样）
    fn wrap(&self, key: &str, text: &str) -> String {
        if (self.is_user() && key == "edith.input") || (self.capability.is_empty() && self.kind == "agent") {
            return text.to_string();
        }
        let from = if self.from.is_empty() { String::new() } else { format!("，说话的是 {}", self.from) };
        let args = [("source", self.source()), ("from", from), ("text", text.to_string())];
        prompts::text(key, &args.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), None)
    }
}

/// 同 Friday v2.92：交来的话后面附上发出时间（edith.now；代替 get_current_time，存进历史、重建时逐字一致）；子智能体交回的不附
pub fn now_note() -> String {
    let now = chrono::Local::now();
    let weekday = crate::tools::weekday(&now);
    prompts::text(
        "edith.now",
        &[(
            "now".to_string(),
            format!("{}年{}月{}日 {weekday} {}", now.format("%Y"), now.format("%-m"), now.format("%-d"), now.format("%H:%M")),
        )]
        .into_iter()
        .collect(),
        None,
    )
}

fn stamped(origin: &Origin, text: String) -> String {
    if origin.kind == "agent" { text } else { format!("{text}\n\n{}", now_note()) }
}

struct Input {
    text: String,
    origin: Origin,
    turn: String,
    /// 补充没赶上那一轮（它直接答完了）、紧接着单独干的：照补充的说法交给模型
    interjected: bool,
}

/// 收到的话怎么处理了
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// 会话闲着：马上开始一轮
    Started,
    /// 打断了正在干的一轮，带着它重来
    Interrupted,
    /// 塞进了正在干的这一轮
    Interjected,
    /// 排在后面
    Queued,
    /// 记进了会话
    Noted,
}

impl Disposition {
    pub fn name(self) -> &'static str {
        match self {
            Disposition::Started => "started",
            Disposition::Interrupted => "interrupted",
            Disposition::Interjected => "interjected",
            Disposition::Queued => "queued",
            Disposition::Noted => "noted",
        }
    }
}

/// 推给收听者的一件事
#[derive(Debug, Clone)]
pub struct Envelope {
    pub session: String,
    pub turn: String,
    /// turn.started / text / thinking / tool / result / stats / notice / done
    pub kind: &'static str,
    pub data: Value,
}

impl Envelope {
    pub fn to_json(&self) -> Value {
        json!({ "session": self.session, "turn": self.turn, "type": self.kind, "data": self.data })
    }
}

type Listener = Arc<dyn Fn(&Envelope) + Send + Sync>;

#[derive(Default)]
struct State {
    working: bool,
    turn: String,
    cancel: Arc<AtomicBool>,
    /// 等着塞进这一轮的（还是原话，塞的时候再包）
    interjections: Vec<Input>,
    queue: VecDeque<Input>,
    /// 干活时来的「只记下」：这一轮存档时记进去
    notes: Vec<String>,
    /// 正在干的这一轮许可的工具（补充 / 打断只在不越过它时生效）
    tools: crate::permissions::Tools,
    /// 用户（或能力）最近交来的原话：交付检查、派给子智能体的背景按它（子智能体交回的不算）
    request: String,
    /// 正在干的这一轮交来的原话（sessions.list 报给后连上来的客户端）
    text: String,
}

/// 这个会话里最近交来的原话（子智能体交回的不算）
pub fn request_of(session: &str) -> String {
    SESSIONS.lock().unwrap().get(session).map(|state| state.request.clone()).unwrap_or_default()
}

static SESSIONS: LazyLock<Mutex<HashMap<String, State>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static LISTENERS: LazyLock<RwLock<Vec<(u64, Listener)>>> = LazyLock::new(|| RwLock::new(Vec::new()));
static NEXT_LISTENER: AtomicU64 = AtomicU64::new(1);
/// 没指明会话的话交给它（终端开的会话、edith run 的主会话）
static FOCUS: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

pub fn focus() -> String {
    FOCUS.lock().unwrap().clone()
}

pub fn set_focus(session: &str) {
    *FOCUS.lock().unwrap() = session.to_string();
}

pub fn listen(listener: impl Fn(&Envelope) + Send + Sync + 'static) -> u64 {
    let id = NEXT_LISTENER.fetch_add(1, Ordering::Relaxed);
    LISTENERS.write().unwrap().push((id, Arc::new(listener)));
    id
}

pub fn unlisten(id: u64) {
    LISTENERS.write().unwrap().retain(|(existing, _)| *existing != id);
}

pub fn emit(session: &str, turn: &str, kind: &'static str, data: Value) {
    let envelope = Envelope { session: session.to_string(), turn: turn.to_string(), kind, data };
    // 先把收听者拿出来再调，调的时候不拿着锁（收听者里登记 / 退订也不会死锁）
    let listeners: Vec<Listener> = LISTENERS.read().unwrap().iter().map(|(_, listener)| listener.clone()).collect();
    for listener in listeners {
        listener(&envelope);
    }
}

pub fn new_turn_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// 会话在干活吗；在干的是哪一轮
pub fn working(session: &str) -> Option<String> {
    SESSIONS.lock().unwrap().get(session).filter(|state| state.working).map(|state| state.turn.clone())
}

/// 排着的（下一轮起做的）：[(轮, 原话)]
pub fn queue_of(session: &str) -> Vec<(String, String)> {
    SESSIONS
        .lock()
        .unwrap()
        .get(session)
        .map(|state| {
            state.queue.iter().filter(|input| input.origin.kind != "agent").map(|input| (input.turn.clone(), input.text.clone())).collect()
        })
        .unwrap_or_default()
}

/// 拿掉一段排着的（还没开始的那一轮）
pub fn dequeue(session: &str, turn: &str) -> bool {
    let mut sessions = SESSIONS.lock().unwrap();
    let Some(state) = sessions.get_mut(session) else { return false };
    let before = state.queue.len();
    state.queue.retain(|input| input.turn != turn);
    state.queue.len() != before
}

/// 在干的那一轮：(轮, 交来的原话)
pub fn current(session: &str) -> Option<(String, String)> {
    SESSIONS.lock().unwrap().get(session).filter(|state| state.working).map(|state| (state.turn.clone(), state.text.clone()))
}

/// 有会话在干活（含派出去的子智能体）
pub fn any_working() -> bool {
    let sessions: Vec<String> = {
        let all = SESSIONS.lock().unwrap();
        if all.values().any(|state| state.working) {
            return true;
        }
        all.keys().cloned().collect()
    };
    sessions.iter().any(|session| crate::team::busy(session))
}

/// 停下这个会话正在干的一轮（排着的也不干了，派出去的子智能体、工作流也停）；没在干返回 false
pub fn stop(session: &str) -> bool {
    crate::team::stop_session(session);
    let mut sessions = SESSIONS.lock().unwrap();
    let Some(state) = sessions.get_mut(session).filter(|state| state.working) else { return false };
    state.queue.clear();
    state.interjections.clear();
    state.cancel.store(true, Ordering::Relaxed);
    true
}

/// 交一段话给大脑。turn：调用方先定好的轮 id（能力要先登记自己等哪一轮；补充进正在干的那轮时不用它）。
/// 返回怎么处理的、算在哪一轮（只记下的没有轮）
pub fn submit(
    session: &str,
    text: &str,
    origin: Origin,
    urgency: Urgency,
    turn: Option<String>,
) -> Result<(Disposition, Option<String>), String> {
    let mut turn = turn;
    let mut new_turn_id = move || turn.take().unwrap_or_else(new_turn_id);
    let text = text.trim();
    if text.is_empty() {
        return Err("text 是空的".into());
    }
    let mut sessions = SESSIONS.lock().unwrap();
    let state = sessions.entry(session.to_string()).or_default();
    if !state.working && Session::load(session).is_none() {
        return Err(format!("没有会话 {session}"));
    }
    if origin.kind != "agent" && urgency != Urgency::Note {
        state.request = text.to_string();
    }
    if urgency == Urgency::Note {
        let note = origin.wrap("edith.note", text);
        if state.working {
            state.notes.push(note);
        } else {
            // 闲着：直接记进存档（拿着锁，不会和刚开始的一轮抢着读写）
            let mut stored = Session::load(session).ok_or_else(|| format!("没有会话 {session}"))?;
            stored.history.push(json!({ "role": "user", "content": note }));
            stored.save()?;
        }
        return Ok((Disposition::Noted, None));
    }
    // 权限和正在干的这一轮不一样：补充不塞进去（塞进去就借了这一轮的工具），打断只有全权的能（不许停下别人的活），都改成排队
    let urgency = match urgency {
        Urgency::Interject if state.working && origin.tools != state.tools => Urgency::Queue,
        Urgency::Interrupt if state.working && origin.tools != crate::permissions::Tools::All => Urgency::Queue,
        other => other,
    };
    if !state.working {
        let input = Input { text: text.to_string(), origin, turn: new_turn_id(), interjected: false };
        let turn = input.turn.clone();
        state.tools = input.origin.tools.clone();
        state.working = true;
        state.turn = turn.clone();
        state.cancel = Arc::new(AtomicBool::new(false));
        let session = session.to_string();
        std::thread::spawn(move || work(session, input));
        return Ok((Disposition::Started, Some(turn)));
    }
    Ok(match urgency {
        Urgency::Interrupt => {
            let input = Input { text: text.to_string(), origin, turn: new_turn_id(), interjected: false };
            let turn = input.turn.clone();
            state.queue.push_front(input);
            state.cancel.store(true, Ordering::Relaxed);
            (Disposition::Interrupted, Some(turn))
        }
        Urgency::Interject => {
            state.interjections.push(Input { text: text.to_string(), origin, turn: state.turn.clone(), interjected: true });
            (Disposition::Interjected, Some(state.turn.clone()))
        }
        _ => {
            let input = Input { text: text.to_string(), origin, turn: new_turn_id(), interjected: false };
            let turn = input.turn.clone();
            state.queue.push_back(input);
            (Disposition::Queued, Some(turn))
        }
    })
}

/// 一个会话的活：一轮接一轮，直到没有排着的
fn work(session_id: String, first: Input) {
    let mut input = first;
    loop {
        let cancel = {
            let mut sessions = SESSIONS.lock().unwrap();
            let state = sessions.entry(session_id.clone()).or_default();
            state.turn = input.turn.clone();
            state.text = input.text.clone();
            state.tools = input.origin.tools.clone();
            state.cancel.clone()
        };
        let turn = input.turn.clone();
        emit(&session_id, &turn, "turn.started", json!({ "text": input.text, "origin": input.origin.to_json() }));
        let done = run_turn(&session_id, &input, &cancel);
        emit(&session_id, &turn, "done", done);
        // 下一轮：没塞进去的补充（这一轮直接答完了）先，再是排着的
        let mut sessions = SESSIONS.lock().unwrap();
        let state = sessions.entry(session_id.clone()).or_default();
        state.cancel = Arc::new(AtomicBool::new(false));
        if !state.interjections.is_empty() {
            let leftovers: Vec<Input> = std::mem::take(&mut state.interjections);
            let origin = leftovers[0].origin.clone();
            let text = leftovers.iter().map(|item| item.text.as_str()).collect::<Vec<_>>().join("\n\n");
            input = Input { text, origin, turn: new_turn_id(), interjected: true };
            continue;
        }
        match state.queue.pop_front() {
            // 排着的几段都是子智能体交回的：并成一轮验收（不然每段各开一轮，整段上下文发好几遍）
            Some(mut next) if next.origin.kind == "agent" => {
                while state
                    .queue
                    .front()
                    .is_some_and(|following| following.origin.kind == "agent" && following.origin.tools == next.origin.tools)
                {
                    let following = state.queue.pop_front().unwrap();
                    next.text = format!("{}\n\n{}", next.text, following.text);
                }
                input = next;
            }
            Some(next) => input = next,
            None => {
                state.working = false;
                state.turn.clear();
                return;
            }
        }
    }
}

fn run_turn(session_id: &str, input: &Input, cancel: &Arc<AtomicBool>) -> Value {
    let failed = |message: String| json!({ "answer": "", "stopped": false, "error": message });
    let Some(mut session) = Session::load(session_id) else { return failed(format!("没有会话 {session_id}")) };
    // 标题用原话（交给模型的那段带着来源说明）
    if session.title.is_empty() {
        session.title = input.text.replace('\n', " ").chars().take(40).collect();
    }
    let model = match Model::load_engine(input.origin.prefs.engine.as_deref()) {
        Ok(model) => model,
        Err(message) => return failed(message),
    };
    let cwd = PathBuf::from(&session.cwd);
    let cwd = if cwd.is_dir() { cwd } else { crate::home::user_home() };
    // v2.86 存档里是原样的历史：每轮开头成批整理较早的部分怎么交给模型（folds.rs），照标记拼出这一轮的那份
    let echoes = crate::folds::echoes_reasoning(&model.base_url, &model.model);
    let change = crate::folds::fold(&session.history, &mut session.folds, echoes);
    let turn_number = crate::folds::next_turn(&session.history);
    let pending = Arc::new(Mutex::new(Vec::<Value>::new()));
    let context = brain::Context {
        session: session_id.to_string(),
        prefs: input.origin.prefs.clone(),
        turn: input.turn.clone(),
        tools: input.origin.tools.clone(),
        origin: input.origin.capability.clone(),
        cancel: Some(cancel.clone()),
        me: None,
        instructions: session.instructions.clone(),
        history_turn: Some(turn_number),
        prior_calls: Some(Arc::new(brain::calls_with_folds(&session.history, &session.folds))),
        pending: Some(pending.clone()),
    };
    let recorder = Mutex::new(crate::checks::Recorder::default());
    let mut text =
        stamped(&input.origin, input.origin.wrap(if input.interjected { "edith.interjection" } else { "edith.input" }, &input.text));
    // 用户发消息时的钩子（能力管的 UserPromptSubmit，Friday 的 hooks.json）：拦下就不交给模型；补的上下文接在这段后面
    if input.origin.kind != "agent" && crate::capabilities::has_hook("UserPromptSubmit") {
        let fields = json!({ "session_id": session_id, "cwd": cwd.to_string_lossy(), "prompt": input.text });
        let outcome = crate::capabilities::run_hook("UserPromptSubmit", fields);
        if outcome.block || outcome.permission.as_deref() == Some("deny") {
            return failed(format!("用户配的钩子拦下了这条消息，没有发给智能体：{}", outcome.reason));
        }
        if !outcome.context.is_empty() {
            let note = prompts::text("hooks.prompt", &[("context".to_string(), outcome.context.join("\n"))].into_iter().collect(), None);
            text = format!("{text}\n\n{note}");
        }
    }
    let mut on_event = |event: Event| {
        recorder.lock().unwrap().on(&event);
        let (kind, data) = event_json(&event);
        emit(session_id, &input.turn, kind, data);
    };
    // 工具跑完、下一段请求之前：把等着的补充塞进去
    let mut interject = || {
        let taken: Vec<Input> = {
            let mut sessions = SESSIONS.lock().unwrap();
            sessions.get_mut(session_id).map(|state| std::mem::take(&mut state.interjections)).unwrap_or_default()
        };
        // 一条条塞进去的那一刻告诉界面（Friday 网关把它当一条用户消息接在这一轮里）
        for item in &taken {
            emit(session_id, &input.turn, "input", json!({ "text": item.text, "origin": item.origin.to_json() }));
        }
        taken.iter().map(|item| stamped(&item.origin, item.origin.wrap("edith.interjection", &item.text))).collect::<Vec<_>>()
    };
    if !change.is_empty() {
        emit(session_id, &input.turn, "notice", json!({ "text": change.notice() }));
    }
    let history = crate::folds::render(&session.history, &session.folds, echoes);
    let mut turn = brain::run(&model, history, &text, &input.origin.images, &cwd, cancel, &context, &mut on_event, &mut interject);
    pending.lock().unwrap().extend(turn.appended.iter().cloned());
    // 交付检查（checks.rs）：写完没验证、交的是方案、只说了下一步、问要不要修——追一条提醒接着做，每种最多一次
    let request = request_of(session_id);
    let delegates = context.tools.allows("delegate_task", false);
    let mut checked = std::collections::HashSet::new();
    // 计划模式交的就是计划：不追「只交了方案」这类提醒（同 Friday）
    while !turn.stopped && turn.error.is_none() && !cancel.load(Ordering::Relaxed) && !context.prefs.plan() {
        let followup = crate::checks::main_followup(&request, &recorder.lock().unwrap().blocks, delegates, &checked);
        let Some(followup) = followup else { break };
        checked.insert(followup.kind);
        emit(session_id, &input.turn, "notice", json!({ "text": followup.notice }));
        let previous = turn.answer.clone();
        turn = brain::run(&model, turn.history, &followup.prompt, &[], &cwd, cancel, &context, &mut on_event, &mut interject);
        pending.lock().unwrap().extend(turn.appended.iter().cloned());
        if turn.answer.trim().is_empty() {
            turn.answer = previous;
        }
    }
    // 一轮结束时的钩子（能力管的 Stop）：不让停就把它说的当一条用户消息接着做，最多 3 次（同 Friday）
    for round in 0..3 {
        if turn.stopped || turn.error.is_some() || cancel.load(Ordering::Relaxed) || !crate::capabilities::has_hook("Stop") {
            break;
        }
        let fields = json!({
            "session_id": session_id,
            "cwd": cwd.to_string_lossy(),
            "stop_hook_active": round > 0,
            "last_assistant_message": turn.answer,
        });
        let outcome = crate::capabilities::run_hook("Stop", fields);
        if !outcome.block {
            break;
        }
        let first = outcome.reason.lines().next().unwrap_or("").to_string();
        emit(session_id, &input.turn, "notice", json!({ "text": format!("一轮结束的钩子让智能体接着做：{first}") }));
        let prompt = prompts::text("hooks.stop", &[("reason".to_string(), outcome.reason)].into_iter().collect(), None);
        let previous = turn.answer.clone();
        turn = brain::run(&model, turn.history, &prompt, &[], &cwd, cancel, &context, &mut on_event, &mut interject);
        pending.lock().unwrap().extend(turn.appended.iter().cloned());
        if turn.answer.trim().is_empty() {
            turn.answer = previous;
        }
    }
    // 存原样的：这一轮接上的全部（第一条是用户交来的那条，记上第几轮）
    let mut appended = std::mem::take(&mut *pending.lock().unwrap());
    if let Some(first) = appended.first_mut().filter(|first| first["role"] == "user") {
        first[crate::folds::TURN] = json!(turn_number);
    }
    session.history.extend(appended);
    let notes: Vec<String> = {
        let mut sessions = SESSIONS.lock().unwrap();
        sessions.get_mut(session_id).map(|state| std::mem::take(&mut state.notes)).unwrap_or_default()
    };
    session.history.extend(notes.into_iter().map(|note| json!({ "role": "user", "content": note })));
    let saved = session.save().err();
    json!({ "answer": turn.answer, "stopped": turn.stopped, "error": turn.error.or(saved) })
}

pub fn event_json(event: &Event) -> (&'static str, Value) {
    match event {
        Event::Text(text) => ("text", json!({ "text": text })),
        Event::Thinking(text) => ("thinking", json!({ "text": text })),
        Event::Tool { id, name, arguments } => ("tool", json!({ "id": id, "name": name, "arguments": arguments })),
        Event::Result { id, name, ok, text, lines } => {
            let mut data = json!({ "id": id, "name": name, "ok": ok, "text": text });
            if let Some([added, removed]) = lines {
                data["lines"] = json!({ "added": added, "removed": removed });
            }
            ("result", data)
        }
        Event::Stats(stats) => ("stats", stats.clone()),
        Event::Notice(text) => ("notice", json!({ "text": text })),
    }
}

/// 交一段话、等它那一轮做完（ask）：先登记收听再交，免得做得太快错过结果。返回怎么处理的、哪一轮、done 的内容
pub fn ask(
    session: &str,
    text: &str,
    origin: Origin,
    urgency: Urgency,
    turn: Option<String>,
) -> Result<(Disposition, Option<String>, Value), String> {
    let (sender, receiver) = std::sync::mpsc::channel::<(String, Value)>();
    let sender = Mutex::new(sender);
    let id = listen(move |envelope| {
        if envelope.kind == "done" {
            let _ = sender.lock().unwrap().send((envelope.turn.clone(), envelope.data.clone()));
        }
    });
    let submitted = submit(session, text, origin, urgency, turn);
    let (disposition, turn) = match submitted {
        Ok(result) => result,
        Err(message) => {
            unlisten(id);
            return Err(message);
        }
    };
    let Some(wanted) = turn.clone() else {
        unlisten(id);
        return Ok((disposition, None, json!({ "answer": "", "stopped": false, "error": null })));
    };
    let value = loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok((done_turn, value)) if done_turn == wanted => break value,
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !pending(&wanted) => {
                break json!({ "answer": "", "stopped": true, "error": "这一轮没有干（会话被停下了）" });
            }
            Err(_) => {}
        }
    };
    unlisten(id);
    Ok((disposition, turn, value))
}

/// 这一轮还在干或者排着
fn pending(turn: &str) -> bool {
    SESSIONS.lock().unwrap().values().any(|state| {
        (state.working && state.turn == turn)
            || state.queue.iter().any(|input| input.turn == turn)
            || state.interjections.iter().any(|input| input.turn == turn)
    })
}

/// 等到这个会话闲下来（命令行干完一件事再退出：中途补充、排着的也干完）
pub fn wait_idle(session: &str) {
    // 派出去的子智能体、工作流还没交回的也等（交回后会再干一轮验收）
    while working(session).is_some() || crate::team::busy(session) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
