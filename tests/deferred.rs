//! 按需加载：工具定义只给一直要用的，MCP / 能力 / 派子智能体的其余工具先只列名字、load_tools 取回才给；MCP 服务用到时才起；技能只列名字和用途

mod common;

use common::{command, fake, fake_model, python, scratch, text};
use serde_json::{Value, json};

fn requests(home: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"].as_array().into_iter().flatten().filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string)).collect()
}

#[test]
fn tools_load_on_demand() {
    let (work, home) = scratch("deferred");
    let model = fake_model(&home.join("requests.jsonl"));
    let config = json!({ "mcpServers": { "demo": { "command": python(), "args": ["-u", fake("limb.py").to_str().unwrap()] } } });
    std::fs::write(work.join(".mcp.json"), config.to_string()).unwrap();
    assert!(common::edith(&home, model.port, &work, &["trust", "--yes"]).status.success());
    // 没连过的服务：起来时不连，清单里说还不知道有哪些工具；第一段请求里没有它的工具，也没有工作流这些
    let output = command(&home, model.port, &work, &[r#"调用工具 load_tools 参数 {"names":["demo"]}"#]).output().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let sent = requests(&home);
    let first = &sent[0];
    let names = tool_names(first);
    assert!(names.contains(&"delegate_task".to_string()) && names.contains(&"load_tools".to_string()), "{names:?}");
    assert!(!names.iter().any(|name| name == "run_workflow" || name == "demo__echo"), "{names:?}");
    let system = first["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("按需加载的工具") && system.contains("run_workflow") && system.contains("MCP 服务「demo」：还没连过"),
        "{system}"
    );
    // load_tools 连上它、取回工具；下一段请求就有了，缓存也记下了
    assert!(String::from_utf8_lossy(&output.stdout).contains("已取回 1 个工具"), "{}", text(&output));
    assert!(tool_names(&sent[1]).contains(&"demo__echo".to_string()));
    assert!(std::fs::read_to_string(home.join("mcp-cache.json")).unwrap().contains("echo"));
    // 再开一次：清单用缓存的（列出工具名，不用先连）；直接调也行，用到时才起
    let output = command(&home, model.port, &work, &["用四肢回个声"]).output().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("四肢说：四肢回声：hi"), "{}", text(&output));
    let last_session_first = requests(&home).into_iter().rev().find(|request| request["messages"].as_array().unwrap().len() == 2).unwrap();
    assert!(last_session_first["messages"][0]["content"].as_str().unwrap().contains("demo__echo（原样返回 text）"));
}

#[test]
fn skills_are_listed_and_read_on_demand() {
    let (work, home) = scratch("skills");
    let model = fake_model(&home.join("requests.jsonl"));
    // EDITH 自己的和 Claude Code 的技能都认
    let user = work.join("user");
    std::fs::create_dir_all(home.join("skills").join("release")).unwrap();
    std::fs::write(home.join("skills/release/SKILL.md"), "---\nname: release\ndescription: 发版的步骤\n---\n先跑测试，再打标签 v1。")
        .unwrap();
    std::fs::create_dir_all(user.join(".claude/skills/pdf")).unwrap();
    std::fs::write(user.join(".claude/skills/pdf/SKILL.md"), "---\nname: pdf\ndescription: 处理 PDF\n---\n用 pdftotext。").unwrap();
    let run = |task: &str| command(&home, model.port, &work, &[task]).env("HOME", &user).env("USERPROFILE", &user).output().unwrap();
    let listed = run("skills");
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("release（EDITH）")
            && String::from_utf8_lossy(&listed.stdout).contains("pdf（Claude Code）")
    );
    let output = run(r#"调用工具 use_skill 参数 {"name":"release"}"#);
    assert!(String::from_utf8_lossy(&output.stdout).contains("先跑测试，再打标签 v1"), "{}", text(&output));
    let first = &requests(&home)[0];
    let system = first["messages"][0]["content"].as_str().unwrap();
    // 清单里只有名字和用途，全文用到时才读
    assert!(system.contains("- release：发版的步骤") && system.contains("- pdf：处理 PDF") && !system.contains("pdftotext"), "{system}");
    assert!(tool_names(first).contains(&"use_skill".to_string()));
}
