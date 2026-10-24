//! 交付检查、子智能体、工作流：假模型（tests/fake/model.py）按剧本走

mod common;

use common::{edith, fake_model, scratch, text};

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
