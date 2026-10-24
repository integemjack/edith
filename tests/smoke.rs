//! 端到端：假模型（tests/fake/model.py，OpenAI 兼容的流式接口）+ 假能力（tests/fake/cap.py）跑 edith 命令行

mod common;

use common::{edith, fake, fake_model, python, scratch, text};

#[test]
fn writes_checks_and_delivers() {
    let (work, home) = scratch("work");
    let model = fake_model(&home.join("requests.jsonl"));
    let output = edith(&home, model.port, &work, &["-C", work.to_str().unwrap(), "写个 hello.txt 然后核对"]);
    assert!(output.status.success(), "{}", text(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("做完了：hello.txt 已写好"), "{}", text(&output));
    assert_eq!(std::fs::read_to_string(work.join("hello.txt")).unwrap(), "你好 EDITH\n");
    // 会话存下了，系统提示里没有缺的参数
    let sessions = edith(&home, model.port, &work, &["sessions"]);
    assert!(String::from_utf8_lossy(&sessions.stdout).contains("写个 hello.txt 然后核对"), "{}", text(&sessions));
    let requests = std::fs::read_to_string(home.join("requests.jsonl")).unwrap();
    assert_eq!(requests.lines().count(), 3);
    assert!(!requests.contains("【缺"), "提示词缺参数：{requests}");
}

#[test]
fn capability_add_and_remove() {
    let (work, home) = scratch("caps");
    let model = fake_model(&home.join("requests.jsonl"));
    let cap = fake("cap.py");
    // 加：大脑起它、它连进套接字，连得上才记进配置
    let added = edith(&home, model.port, &work, &["capabilities", "add", "cap", python(), "-u", cap.to_str().unwrap()]);
    assert!(added.status.success() && String::from_utf8_lossy(&added.stdout).contains("接上了「cap」：1 个动作"), "{}", text(&added));
    let listed = edith(&home, model.port, &work, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("cap（本地） 测试能力"), "{}", text(&listed));
    // 起不来的不记
    let broken = edith(&home, model.port, &work, &["capabilities", "add", "bad", python(), "-c", "import sys; sys.exit(3)"]);
    assert!(!broken.status.success() && String::from_utf8_lossy(&broken.stderr).contains("退出了"), "{}", text(&broken));
    let removed = edith(&home, model.port, &work, &["capabilities", "remove", "cap"]);
    assert!(removed.status.success(), "{}", text(&removed));
    let listed = edith(&home, model.port, &work, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("还没有能力"), "{}", text(&listed));
}

#[test]
fn mcp_servers_come_from_config() {
    let (work, home) = scratch("mcp");
    let model = fake_model(&home.join("requests.jsonl"));
    let limb = fake("limb.py");
    // 项目的 .mcp.json（和 Claude Code 一样的写法）：大脑直接连，不用做成能力
    let config = serde_json::json!({ "mcpServers": { "demo": { "command": python(), "args": ["-u", limb.to_str().unwrap()] } } });
    std::fs::write(work.join(".mcp.json"), config.to_string()).unwrap();
    // 项目的配置要先信任（里面的命令会被起）
    assert!(edith(&home, model.port, &work, &["trust", "--yes"]).status.success());
    let listed = edith(&home, model.port, &work, &["mcp"]);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("demo（项目）") && stdout.contains("工具：echo"), "{}", text(&listed));
    let output = edith(&home, model.port, &work, &["用四肢回个声"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("四肢说：四肢回声：hi"), "{}", text(&output));
    // 命令行加的默认进本地那层
    let added = edith(&home, model.port, &work, &["mcp", "add", "other", python(), "-u", limb.to_str().unwrap()]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(std::fs::read_to_string(work.join(".edith").join("mcp.local.json")).unwrap().contains("other"));
    let removed = edith(&home, model.port, &work, &["mcp", "remove", "other"]);
    assert!(removed.status.success(), "{}", text(&removed));
    assert!(!std::fs::read_to_string(work.join(".edith").join("mcp.local.json")).unwrap().contains("other"));
}

#[test]
fn searches_and_reads_the_web() {
    let (work, home) = scratch("web");
    let model = fake_model(&home.join("requests.jsonl"));
    let base = format!("http://127.0.0.1:{}", model.port);
    let run = |task: &str| {
        common::command(&home, model.port, &work, &[task])
            .env("EDITH_BING_URL", format!("{base}/bing"))
            .env("EDITH_DDG_URL", format!("{base}/ddg"))
            .env("EDITH_BAIDU_URL", format!("{base}/baidu"))
            .output()
            .unwrap()
    };
    // 三家一起查、轮流合并、去重
    let output = run(r#"调用工具 web_search 参数 {"query":"rust"}"#);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for wanted in ["Bing 的结果", "必应摘要", "DDG 的结果", "https://ddg.example/b", "百度的结果", "https://baidu.example/c"]
    {
        assert!(stdout.contains(wanted), "少了 {wanted}：{}", text(&output));
    }
    assert_eq!(stdout.matches("重复的").count(), 1, "{}", text(&output));
    // GBK 的中文网页：标题、正文、编号的链接
    let output = run(&format!(r#"调用工具 fetch_url 参数 {{"url":"{base}/page"}}"#));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for wanted in ["# 中文页", "# 标题一", "正文段落，下一页[1]", &format!("[1] 下一页 {base}/next")] {
        assert!(stdout.contains(wanted), "少了 {wanted}：{}", text(&output));
    }
}
