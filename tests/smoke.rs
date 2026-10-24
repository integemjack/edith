//! 端到端：假模型（tests/fake/model.py，OpenAI 兼容的流式接口）+ 假四肢（tests/fake/limb.py，stdio MCP）跑 edith 命令行

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn fake(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fake").join(name)
}

/// 起假模型，读它挑的端口；测试结束时结束它
struct FakeModel {
    child: Child,
    port: u16,
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn fake_model(log: &Path) -> FakeModel {
    let mut child = Command::new(python())
        .arg("-u")
        .arg(fake("model.py"))
        .arg("0")
        .arg(log)
        .stdout(Stdio::piped())
        .spawn()
        .expect("起不了假模型（要 python3）");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    FakeModel { port: line.trim().parse().expect("假模型没打出端口"), child }
}

/// 一次测试的目录：work（工作目录）、home（EDITH_HOME）
fn scratch(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("edith-smoke-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (work, home) = (root.join("work"), root.join("home"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    (work, home)
}

fn edith(home: &Path, port: u16, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_edith"))
        .args(args)
        .current_dir(cwd)
        .env("EDITH_HOME", home)
        .env("EDITH_BASE_URL", format!("http://127.0.0.1:{port}/v1"))
        .env("EDITH_MODEL", "fake")
        .env("EDITH_API_KEY", "none")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!("{}\n--- stderr ---\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

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
fn limbs_attach_and_answer() {
    let (work, home) = scratch("limbs");
    let model = fake_model(&home.join("requests.jsonl"));
    let limb = fake("limb.py");
    let added = edith(&home, model.port, &work, &["limbs", "add", "demo", python(), "-u", limb.to_str().unwrap()]);
    assert!(added.status.success() && String::from_utf8_lossy(&added.stdout).contains("1 个工具"), "{}", text(&added));
    let output = edith(&home, model.port, &work, &["-C", work.to_str().unwrap(), "用四肢打个招呼"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("四肢说：四肢回声：hi"), "{}", text(&output));
    let removed = edith(&home, model.port, &work, &["limbs", "remove", "demo"]);
    assert!(removed.status.success(), "{}", text(&removed));
    let listed = edith(&home, model.port, &work, &["limbs"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("还没有挂四肢"), "{}", text(&listed));
}
