//! 能力协议端到端（docs/CAPABILITY-PROTOCOL.md）：假模型 + 假能力（tests/fake/cap.py，大脑起它、它连进套接字）

mod common;

use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use common::{command, edith, fake, fake_model, python, scratch, text};
use serde_json::{Value, json};

/// 写 capabilities.json：假能力，script 是它握手后按时发的消息
fn configure_cap(home: &Path, script: Value, receives: &str) {
    configure_cap_with(home, script, receives, 0.0);
}

fn configure_cap_with(home: &Path, script: Value, receives: &str, act_delay: f64) {
    let config = json!({ "capabilities": [{
        "name": "cap",
        "command": python(),
        "args": ["-u", fake("cap.py").to_str().unwrap()],
        "env": {
            "CAP_LOG": home.join("cap.jsonl").to_str().unwrap(),
            "CAP_SCRIPT": script.to_string(),
            "CAP_RECEIVES": receives,
            "CAP_ACT_DELAY": act_delay.to_string(),
        },
    }]});
    std::fs::write(home.join("capabilities.json"), config.to_string()).unwrap();
}

/// 假能力收到的消息
fn received(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("cap.jsonl")).unwrap_or_default().lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

fn events<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|message| message["method"] == "event" && message["params"]["type"] == kind)
        .map(|message| &message["params"])
        .collect()
}

fn response(messages: &[Value], id: i64) -> Option<&Value> {
    messages.iter().find(|message| message["id"] == id && message.get("method").is_none())
}

/// 等到条件成立（最多 seconds 秒）
fn wait_until(seconds: u64, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 后台跑 edith run；测试完结束它
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(home: &Path, port: u16, work: &Path) -> Running {
    let child = command(home, port, work, &["run", "-C", work.to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    Running(child)
}

fn sense(text: &str, urgency: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": "sense", "params": { "text": text, "kind": "speech", "from": "小明", "urgency": urgency } })
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

#[test]
fn action_called_with_progress() {
    let (work, home) = scratch("act");
    let model = fake_model(&home.join("requests.jsonl"));
    configure_cap(&home, json!([]), "done");
    let output = edith(&home, model.port, &work, &["-C", work.to_str().unwrap(), "用能力记一下"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("能力说：记住了：hi"), "{}", text(&output));
    // 进度推成了提示；动作带着会话和轮
    assert!(String::from_utf8_lossy(&output.stderr).contains("能力「cap」：在记（50%）"), "{}", text(&output));
    let messages = received(&home);
    let act = messages.iter().find(|message| message["method"] == "act").expect("没收到 act");
    assert_eq!(act["params"]["action"], "remember");
    assert_eq!(act["params"]["arguments"]["text"], "hi");
    assert!(act["params"]["session"].as_str().is_some_and(|session| !session.is_empty()));
    // 握手：EDITH 先发 initialize，再发 initialized
    assert_eq!(messages[0]["method"], "initialize");
    assert_eq!(messages[0]["params"]["protocolVersion"], "1");
    assert_eq!(messages[1]["method"], "initialized");
}

#[test]
fn sense_starts_a_turn_and_notes() {
    let (work, home) = scratch("sense");
    let model = fake_model(&home.join("requests.jsonl"));
    configure_cap(
        &home,
        json!([
            { "delay": 0.3, "send": sense("只聊天", "queue") },
            { "delay": 1.5, "send": request(7, "sense", json!({ "text": "明天下雨", "urgency": "note" })) },
            { "delay": 0.3, "send": request(8, "sessions.list", json!({})) },
        ]),
        "done,turn.started",
    );
    let _edith = run(&home, model.port, &work);
    assert!(wait_until(30, || response(&received(&home), 8).is_some()), "等不到能力的消息：{:?}", received(&home));
    let messages = received(&home);
    let started = events(&messages, "turn.started");
    assert_eq!(started[0]["data"]["origin"]["capability"], "cap");
    assert_eq!(started[0]["data"]["origin"]["from"], "小明");
    let done = events(&messages, "done");
    assert_eq!(done[0]["data"]["answer"], "你好，我是 EDITH。");
    assert_eq!(response(&messages, 7).unwrap()["result"]["disposition"], "noted");
    let sessions = &response(&messages, 8).unwrap()["result"];
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    // 模型看到的是标了来源的话；只记下的进了会话
    let requests = std::fs::read_to_string(home.join("requests.jsonl")).unwrap();
    assert!(requests.contains("由能力「cap」（speech）传来，说话的是 小明"), "{requests}");
    let id = sessions[0]["id"].as_str().unwrap();
    let stored = std::fs::read_to_string(work.join(".edith").join("sessions").join(format!("{id}.json"))).unwrap();
    assert!(stored.contains("明天下雨") && stored.contains("不用回复"), "{stored}");
}

#[test]
fn interrupt_restarts_with_new_words() {
    let (work, home) = scratch("interrupt");
    let model = fake_model(&home.join("requests.jsonl"));
    configure_cap(
        &home,
        json!([
            { "delay": 0.3, "send": request(1, "ask", json!({ "text": "慢慢数" })) },
            { "delay": 1.5, "send": request(2, "sense", json!({ "text": "只聊天", "urgency": "interrupt" })) },
        ]),
        "done",
    );
    let _edith = run(&home, model.port, &work);
    assert!(wait_until(30, || events(&received(&home), "done").len() >= 2), "{:?}", received(&home));
    let messages = received(&home);
    let asked = &response(&messages, 1).expect("ask 没回话")["result"];
    assert_eq!(asked["stopped"], true, "{asked}");
    assert_eq!(response(&messages, 2).unwrap()["result"]["disposition"], "interrupted");
    let done = events(&messages, "done");
    assert_eq!(done[0]["data"]["stopped"], true);
    assert_eq!(done[1]["data"]["answer"], "你好，我是 EDITH。");
}

#[test]
fn interject_joins_the_running_turn() {
    let (work, home) = scratch("interject");
    let model = fake_model(&home.join("requests.jsonl"));
    configure_cap(
        &home,
        json!([
            { "delay": 0.3, "send": request(1, "ask", json!({ "text": "慢慢数" })) },
            { "delay": 1.5, "send": request(2, "sense", json!({ "text": "只聊天", "urgency": "interject" })) },
        ]),
        "done",
    );
    let _edith = run(&home, model.port, &work);
    assert!(wait_until(30, || events(&received(&home), "done").len() >= 2), "{:?}", received(&home));
    let messages = received(&home);
    // 补充进了正在干的那一轮；那一轮直接答完了，紧接着为补充再干一轮
    assert_eq!(response(&messages, 2).unwrap()["result"]["disposition"], "interjected");
    let asked = &response(&messages, 1).expect("ask 没回话")["result"];
    assert_eq!(asked["stopped"], false);
    assert!(asked["answer"].as_str().unwrap().starts_with("0 1 2"), "{asked}");
    let done = events(&messages, "done");
    assert_eq!(done[1]["data"]["answer"], "你好，我是 EDITH。");
    let requests = std::fs::read_to_string(home.join("requests.jsonl")).unwrap();
    assert!(requests.contains("在你工作时补充了下面这段"), "{requests}");
}

#[test]
fn interject_between_steps() {
    let (work, home) = scratch("between");
    let model = fake_model(&home.join("requests.jsonl"));
    // 动作要做 2 秒：补充在它做的时候来，跑完工具、下一段请求之前塞进去，不另起一轮
    configure_cap_with(
        &home,
        json!([
            { "delay": 0.3, "send": request(1, "ask", json!({ "text": "用能力记一下" })) },
            { "delay": 0.8, "send": request(2, "sense", json!({ "text": "顺便记得带伞", "urgency": "interject" })) },
        ]),
        "done,notice",
        2.0,
    );
    let _edith = run(&home, model.port, &work);
    assert!(wait_until(30, || response(&received(&home), 1).is_some()), "{:?}", received(&home));
    std::thread::sleep(Duration::from_millis(500));
    let messages = received(&home);
    assert_eq!(response(&messages, 2).unwrap()["result"]["disposition"], "interjected");
    assert_eq!(response(&messages, 1).unwrap()["result"]["answer"], "能力说：记住了：hi");
    assert_eq!(events(&messages, "done").len(), 1, "补充不该另起一轮");
    assert!(events(&messages, "notice").iter().any(|notice| notice["data"]["text"].as_str().unwrap_or("").contains("收到 1 段补充")));
    let requests: Vec<String> = std::fs::read_to_string(home.join("requests.jsonl")).unwrap().lines().map(str::to_string).collect();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("在你工作时补充了下面这段") && requests[1].contains("顺便记得带伞"), "{}", requests[1]);
}
