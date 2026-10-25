//! 交付检查、子智能体、工作流：假模型（tests/fake/model.py）按剧本走

mod common;

use common::{edith, fake_model, scratch, text};
use serde_json::Value;

/// 存下来给界面看的子 Agent（<项目数据目录>/agents/<会话>/<id>.json），按派它的那一批里的先后
fn agent_records(home: &std::path::Path, work: &std::path::Path) -> (String, Vec<Value>) {
    let sessions = common::read_sessions(home, work);
    let session = sessions.iter().find(|session| session["history"].as_array().is_some_and(|history| !history.is_empty())).unwrap();
    let id = session["id"].as_str().unwrap().to_string();
    let dir = common::project_dir(home, work).join("agents").join(&id);
    let mut records: Vec<Value> = std::fs::read_dir(&dir)
        .unwrap_or_else(|_| panic!("没存子 Agent 的运行记录：{}", dir.display()))
        .flatten()
        .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap())
        .collect();
    records.sort_by_key(|record| record["slot"].as_u64());
    (id, records)
}

/// 主会话里调这个工具的调用 id
fn call_id(home: &std::path::Path, work: &std::path::Path, tool: &str) -> String {
    let sessions = common::read_sessions(home, work);
    sessions
        .iter()
        .flat_map(|session| session["history"].as_array().cloned().unwrap_or_default())
        .flat_map(|message| message["tool_calls"].as_array().cloned().unwrap_or_default())
        .find(|call| call["function"]["name"] == tool)
        .and_then(|call| call["id"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("主会话里没有调 {tool}"))
}

fn requests(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("requests.jsonl")).unwrap_or_default()
}

#[test]
fn delivery_check_pushes_plans_to_results() {
    let (work, home) = scratch("delivery");
    let model = fake_model(&home.join("requests.jsonl"));
    // 交来的是要做的事，回的却是方案：追一条交付检查，接着做完
    let output = edith(&home, model.port, &work, &["交付测试"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("做完了：结果已经交付"), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("像是方案"), "{}", text(&output));
    assert!(requests(&home).contains("交付检查"));
}

#[test]
fn unverified_writes_get_checked() {
    let (work, home) = scratch("verify");
    let model = fake_model(&home.join("requests.jsonl"));
    // 写了 s.py 没运行就说写好了：提醒先验证
    let output = edith(&home, model.port, &work, &["验证测试"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("验证过了"), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("写了 s.py 之后没有运行"), "{}", text(&output));
}

#[test]
fn delegated_work_comes_back_for_review() {
    let (work, home) = scratch("delegate");
    let model = fake_model(&home.join("requests.jsonl"));
    // 主智能体派到后台、这一轮先结束；子智能体做完交回，主智能体再干一轮验收（命令行等它们都交回再退出）
    let output = edith(&home, model.port, &work, &["派活测试"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("已在后台派出子 Agent「小助手」"), "{}", text(&output));
    assert!(stdout.contains("验收通过：子任务做完了"), "{}", text(&output));
    assert!(stderr.contains("子 Agent「小助手」开始") && stderr.contains("交回了结果"), "{}", text(&output));
    // 子智能体看到的：它自己的系统提示、任务、用户的原话
    let log = requests(&home);
    assert!(log.contains("临时子 Agent「小助手」") && log.contains("用户的原话：派活测试"), "{log}");
    // 给界面看的：存了运行记录，记着是哪个调用派的
    let (_, records) = agent_records(&home, &work);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(
        (record["name"].as_str(), record["kind"].as_str(), record["state"].as_str()),
        (Some("小助手"), Some("delegate"), Some("done"))
    );
    assert_eq!(record["call"].as_str(), Some(call_id(&home, &work, "delegate_task").as_str()));
    assert!(record["parent"].is_null() && record["started"].is_string() && record["finished"].is_string(), "{record}");
    let transcript = record["transcript"].as_array().unwrap();
    assert!(
        transcript.iter().any(|piece| piece["type"] == "text" && piece["text"].as_str().is_some_and(|text| !text.is_empty())),
        "{record}"
    );
}

#[test]
fn ready_made_subagents_bring_role_and_tools() {
    let (work, home) = scratch("subagent-type");
    let model = fake_model(&home.join("requests.jsonl"));
    std::fs::create_dir_all(home.join("agents")).unwrap();
    std::fs::write(
        home.join("agents").join("reviewer.md"),
        "---\nname: reviewer\ndescription: 审代码：看改动有没有 bug\ntools: Read, Bash\nmodel: sonnet\n---\n你是代码审查员，只看不改。\n",
    )
    .unwrap();
    let claude = home.join("no-claude");
    let run = |words: &[&str]| common::command(&home, model.port, &work, words).env("CLAUDE_CONFIG_DIR", &claude).output().unwrap();
    let output = run(&["现成测试"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("验收通过：现成的做完了"), "{}", text(&output));
    let log = requests(&home);
    // 主智能体的系统提示列着它；子智能体的角色是文件正文、工具只有 Read（read_file）、Bash（run_shell），名字用它的
    assert!(log.contains("现成的子智能体") && log.contains("- reviewer：审代码：看改动有没有 bug"), "{log}");
    let sub = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|request| request["messages"][0]["content"].as_str().is_some_and(|system| system.contains("你是代码审查员，只看不改")));
    let sub = sub.unwrap_or_else(|| panic!("子智能体的请求里没有现成的角色：{log}"));
    let mut tools: Vec<&str> = sub["tools"].as_array().unwrap().iter().filter_map(|tool| tool["function"]["name"].as_str()).collect();
    tools.sort();
    assert_eq!(tools, vec!["read_file", "run_shell"], "{sub}");
    let (_, records) = agent_records(&home, &work);
    assert_eq!(records[0]["name"], "reviewer");
    // 点了没有的：回给模型，列出有的
    let (work2, _) = scratch("subagent-type-missing");
    let output = common::command(&home, model.port, &work2, &["现成测试 叫不存在的"]).env("CLAUDE_CONFIG_DIR", &claude).output().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("没有叫「nobody」的现成子智能体。有的：reviewer"), "{}", text(&output));
    // edith agents --json 列出来
    let listed = run(&["agents", "--json"]);
    let list: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!((list[0]["name"].as_str(), list[0]["source"].as_str()), (Some("reviewer"), Some("user")), "{list}");
}

#[test]
fn rework_sends_it_back() {
    let (work, home) = scratch("rework");
    let model = fake_model(&home.join("requests.jsonl"));
    let output = edith(&home, model.port, &work, &["打回测试"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("已打回「小助手」"), "{}", text(&output));
    assert!(stdout.contains("验收通过：改好的也收到了"), "{}", text(&output));
    // 打回的带着它原来的上下文接着改
    let log = requests(&home);
    let rework = log.lines().find(|line| line.contains("上级验收没通过")).expect("没打回");
    assert!(rework.contains("子任务：把这件事做完"), "打回时没带着原来的上下文");
}

#[test]
fn workflow_runs_steps_in_order() {
    let (work, home) = scratch("workflow");
    let model = fake_model(&home.join("requests.jsonl"));
    let output = edith(&home, model.port, &work, &["工作流测试"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(String::from_utf8_lossy(&output.stderr).contains("工作流已在后台启动"), "{}", text(&output));
    assert!(stdout.contains("验收通过：两步都做完了，第二步用上了第一步的结果"), "{}", text(&output));
    // b 等 a 做完才开始
    let log = requests(&home);
    let first = log.find("第一步：先做这个").unwrap();
    let second = log.find("第二步：接着做").unwrap();
    assert!(first < second);
}

#[test]
fn parallel_results_come_back_together() {
    let (work, home) = scratch("parallel");
    let model = fake_model(&home.join("requests.jsonl"));
    // 一次并联派出的都做完才一起交回：一个回来开一轮，每轮都要把整段上下文再发一遍
    let output = edith(&home, model.port, &work, &["并联测试"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("验收通过：2 个一起回来了"), "{}", text(&output));
    let main_turns = requests(&home).lines().filter(|line| !line.contains("你是 EDITH 派出的临时子 Agent")).count();
    // 派、派完收尾、一起验收；交回赶上收尾那一段时并进去（机器慢时）是 2 次，一个回来开一轮才会更多
    assert!((2..=3).contains(&main_turns), "主智能体请求了 {main_turns} 次");
    // 一批两个：同一个调用派的，排第 0、第 1
    let (_, records) = agent_records(&home, &work);
    let call = call_id(&home, &work, "run_parallel_tasks");
    let slots: Vec<(Option<&str>, Option<u64>, Option<&str>)> =
        records.iter().map(|record| (record["call"].as_str(), record["slot"].as_u64(), record["kind"].as_str())).collect();
    assert_eq!(slots, [(Some(call.as_str()), Some(0), Some("parallel")), (Some(call.as_str()), Some(1), Some("parallel"))]);
}

#[test]
fn stdin_that_never_closes_does_not_hang() {
    let (work, home) = scratch("stdin");
    let model = fake_model(&home.join("requests.jsonl"));
    // 从脚本、别的智能体里起的：标准输入是一直不关的管子，给了参数就不该一直等它
    let mut child = common::command(&home, model.port, &work, &["只聊天"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take();
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(20) {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let finished = child.try_wait().unwrap().is_some();
    if !finished {
        let _ = child.kill();
    }
    assert!(finished, "标准输入不关就一直卡着");
}
