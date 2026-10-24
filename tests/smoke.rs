//! 端到端：假模型（tests/fake/model.py，OpenAI 兼容的流式接口）+ 假 MCP 服务（tests/fake/limb.py）跑 edith 命令行

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
fn mcp_capability_attach_and_answer() {
    let (work, home) = scratch("limbs");
    let model = fake_model(&home.join("requests.jsonl"));
    let limb = fake("limb.py");
    let added = edith(&home, model.port, &work, &["capabilities", "add-mcp", "demo", python(), "-u", limb.to_str().unwrap()]);
    assert!(added.status.success() && String::from_utf8_lossy(&added.stdout).contains("1 个动作"), "{}", text(&added));
    let output = edith(&home, model.port, &work, &["-C", work.to_str().unwrap(), "用四肢打个招呼"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("四肢说：四肢回声：hi"), "{}", text(&output));
    let removed = edith(&home, model.port, &work, &["capabilities", "remove", "demo"]);
    assert!(removed.status.success(), "{}", text(&removed));
    let listed = edith(&home, model.port, &work, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("还没有能力"), "{}", text(&listed));
}
