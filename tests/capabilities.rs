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
            // 全权的连接说是用户自己打的：交给模型时不标来源
            { "delay": 0.3, "send": request(9, "sense", json!({ "text": "亲手打的：只聊天", "kind": "user" })) },
            { "delay": 1.5, "send": request(8, "sessions.list", json!({})) },
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
    assert!(requests.contains("\"content\": \"亲手打的：只聊天\""), "{requests}");
    let id = sessions[0]["id"].as_str().unwrap();
    let stored = common::read_session(&common::project_dir(&home, &work).join("sessions").join(format!("{id}.jsonl"))).to_string();
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

#[test]
fn permissions_limit_what_a_connection_can_do() {
    let (work, home) = scratch("permissions");
    let model = fake_model(&home.join("requests.jsonl"));
    let write = r#"调用工具 write_file 参数 {"path":"x.txt","content":"x"}"#;
    let script = json!([
        { "delay": 0.3, "send": request(1, "sessions.create", json!({})) },
        { "delay": 0.3, "send": request(2, "ask", json!({ "text": write })) },
        { "delay": 0.3, "send": request(3, "ask", json!({ "text": "调用工具 get_current_time" })) },
        { "delay": 0.3, "send": request(4, "ask", json!({ "text": format!(r#"调用工具 fetch_url 参数 {{"url":"http://127.0.0.1:{}/page"}}"#, model.port) })) },
    ]);
    // 能看会话、不能建；它交来的话只能用只读的工具；它替「my phone」开的中转连接权限更窄
    let config = json!({ "capabilities": [{
        "name": "cap",
        "command": python(),
        "args": ["-u", fake("cap.py").to_str().unwrap()],
        "env": {
            "CAP_LOG": home.join("cap.jsonl").to_str().unwrap(),
            "CAP_SCRIPT": script.to_string(),
            "CAP_RECEIVES": "done",
            "CAP_RELAY": "my phone",
        },
        "permissions": { "sessions": "read", "tools": "read", "relay": { "sessions": "read" } },
    }]});
    std::fs::write(home.join("capabilities.json"), config.to_string()).unwrap();
    let _edith = run(&home, model.port, &work);
    let relayed =
        |messages: &[Value], id: i64| messages.iter().find(|message| message["relay"]["id"] == id).map(|message| message["relay"].clone());
    assert!(
        wait_until(30, || {
            let messages = received(&home);
            response(&messages, 4).is_some() && relayed(&messages, 901).is_some()
        }),
        "{:?}",
        received(&home)
    );
    let messages = received(&home);
    // 越权的回 NOT_PERMITTED
    assert_eq!(response(&messages, 1).unwrap()["error"]["code"], -32006);
    // 它交来的那一轮：写文件的工具不给模型，硬调也不让
    let answer = response(&messages, 2).unwrap()["result"]["answer"].as_str().unwrap_or("").to_string();
    assert!(answer.contains("没有权限用 write_file"), "{answer}");
    assert!(!work.join("x.txt").exists());
    let requests = std::fs::read_to_string(home.join("requests.jsonl")).unwrap();
    let asked: Value = serde_json::from_str(requests.lines().find(|line| line.contains("x.txt")).unwrap()).unwrap();
    let tools: Vec<&str> = asked["tools"].as_array().unwrap().iter().filter_map(|tool| tool["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"read_file") && !tools.contains(&"write_file") && !tools.contains(&"run_shell"), "{tools:?}");
    assert!(response(&messages, 3).unwrap()["result"]["answer"].as_str().unwrap_or("").starts_with("结果："));
    // 中转的：名字由大脑定（<能力>-<as>），握手里带着它的权限
    let initialize = messages.iter().find(|message| message["relay"]["method"] == "initialize").unwrap();
    assert_eq!(initialize["relay"]["params"]["name"], "cap-my_phone");
    assert_eq!(initialize["relay"]["params"]["grant"]["tools"], "read");
    assert_eq!(relayed(&messages, 900).unwrap()["error"]["code"], -32006);
    assert!(relayed(&messages, 901).unwrap()["result"].is_array());
    // 主连接的握手也带着
    assert_eq!(messages[0]["params"]["name"], "cap");
    assert_eq!(messages[0]["params"]["grant"]["sessions"], "read");
    // 权限不全的是客人：话进它自己名下的会话（不进主人的），只看得到自己名下的
    let sessions: Vec<Value> = common::read_sessions(&home, &work);
    let owned: Vec<&Value> = sessions.iter().filter(|session| session["owner"] == "cap").collect();
    assert_eq!(owned.len(), 1, "{sessions:?}");
    assert!(owned[0]["history"].to_string().contains("x.txt"));
    let owner_session = sessions.iter().find(|session| session.get("owner").is_none()).expect("主人的会话");
    assert!(!owner_session["history"].to_string().contains("x.txt"), "客人的话进了主人的会话");
    assert_eq!(relayed(&messages, 901).unwrap()["result"].as_array().unwrap().len(), 0, "中转的看到了别人的会话");
    // 客人交来的那一轮不能访问本机 / 内网
    let answer = response(&messages, 4).unwrap()["result"]["answer"].as_str().unwrap_or("").to_string();
    assert!(answer.contains("本机 / 内网"), "{answer}");
}

#[test]
fn capability_can_approve() {
    let (work, home) = scratch("approve");
    let model = fake_model(&home.join("requests.jsonl"));
    let write = r#"调用工具 write_file 参数 {"path":"y.txt","content":"y"}"#;
    // 两段各用各的记录文件（Windows 上前一段的能力程序可能还开着文件，删不掉）
    let log = |decision: &str| home.join(format!("cap-{decision}.jsonl"));
    let read = |decision: &str| -> Vec<Value> {
        std::fs::read_to_string(log(decision)).unwrap_or_default().lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    };
    let configure = |decision: &str| {
        let script = json!([{ "delay": 0.3, "send": request(1, "ask", json!({ "text": write })) }]);
        let config = json!({ "capabilities": [{
            "name": "cap",
            "command": python(),
            "args": ["-u", fake("cap.py").to_str().unwrap()],
            "env": {
                "CAP_LOG": log(decision).to_str().unwrap(),
                "CAP_SCRIPT": script.to_string(),
                "CAP_RECEIVES": "done",
                "CAP_APPROVE": decision,
            },
        }]});
        std::fs::write(home.join("capabilities.json"), config.to_string()).unwrap();
    };
    let start = || {
        command(&home, model.port, &work, &["run"])
            .env_remove("EDITH_PERMISSION_MODE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(Running)
            .unwrap()
    };
    // 它交来的那一轮要写文件：问它；它说不行
    configure("deny");
    let edith = start();
    assert!(wait_until(30, || response(&read("deny"), 1).is_some()), "{:?}", read("deny"));
    let messages = read("deny");
    let approve = messages.iter().find(|message| message["method"] == "approve").expect("没问它批准");
    assert!(approve["params"]["summary"].as_str().unwrap().contains("写文件"), "{approve}");
    let answer = response(&messages, 1).unwrap()["result"]["answer"].as_str().unwrap_or("").to_string();
    assert!(answer.contains("用户没批准") && answer.contains("先别动"), "{answer}");
    assert!(!work.join("y.txt").exists());
    drop(edith);
    // 说行：写了
    configure("allow");
    let _edith = start();
    assert!(wait_until(30, || response(&read("allow"), 1).is_some()), "{:?}", read("allow"));
    assert!(work.join("y.txt").exists(), "{:?}", read("allow"));
}
