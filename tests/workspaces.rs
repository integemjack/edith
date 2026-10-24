//! 一个目录一个大脑：数据在 <目录>/.edith、同一个目录再开一次是接到正在跑的那个上、两层能力配置、在跑的大脑的登记

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{command, edith, fake, fake_model, python, scratch, text};
use serde_json::{Value, json};

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

fn session_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join(".edith").join("sessions")).map(|entries| entries.count()).unwrap_or(0)
}

fn registry(home: &Path) -> Vec<Value> {
    std::fs::read_dir(home.join("instances"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| serde_json::from_str(&std::fs::read_to_string(entry.path()).ok()?).ok())
        .collect()
}

#[test]
fn each_directory_keeps_its_own_data() {
    let (work, home) = scratch("dirs");
    let model = fake_model(&home.join("requests.jsonl"));
    let (a, b) = (work.join("a"), work.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    for dir in [&a, &b] {
        let output = edith(&home, model.port, dir, &["只聊天"]);
        assert!(output.status.success(), "{}", text(&output));
    }
    assert_eq!(session_files(&a), 1);
    assert_eq!(session_files(&b), 1);
    // 全局目录里没有会话；子目录里干活用的是往上找到的那个目录
    assert!(!home.join("sessions").exists());
    let sub = a.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let output = edith(&home, model.port, &sub, &["只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(session_files(&a), 2);
    assert!(!sub.join(".edith").exists());
    let listed = edith(&home, model.port, &b, &["sessions"]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout).lines().count(), 1, "{}", text(&listed));
}

/// 后台跑 edith run；测试完结束它
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_run(home: &Path, port: u16, dir: &Path) -> Running {
    let before = registry(home).len();
    let child = command(home, port, dir, &["run"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    assert!(wait_until(20, || registry(home).len() > before), "登记没出现");
    Running(child)
}

fn git(dir: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git").arg("-C").arg(dir).args(arguments).output().unwrap();
    assert!(output.status.success(), "git {arguments:?}：{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn repository(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.txt"), "hi\n").unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["add", "."]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
}

#[test]
fn busy_git_directory_moves_to_a_worktree() {
    let (work, home) = scratch("worktree");
    let model = fake_model(&home.join("requests.jsonl"));
    let repo = std::fs::canonicalize(&work).unwrap();
    repository(&repo);
    let running = start_run(&home, model.port, &repo);
    // 有大脑在干活：新起的去 worktree（在子目录里起的对应到 worktree 的同一个子目录）
    let output = edith(&home, model.port, &repo.join("src"), &["只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("git worktree") && stderr.contains("edith/"), "{}", text(&output));
    let trees: Vec<_> = std::fs::read_dir(repo.join(".edith").join("worktrees")).unwrap().flatten().map(|entry| entry.path()).collect();
    assert_eq!(trees.len(), 1);
    assert_eq!(session_files(&trees[0]), 1);
    assert_eq!(session_files(&repo), 1, "主仓库里只有 run 的主会话");
    let requests = std::fs::read_to_string(home.join("requests.jsonl")).unwrap();
    assert!(requests.contains(&trees[0].join("src").to_string_lossy().replace('\\', "\\\\")), "工作目录该是 worktree 里的 src");
    assert!(git(&repo, &["worktree", "list"]).contains("edith/"));
    // .edith/ 不进 git
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    // --here：不开
    let output = edith(&home, model.port, &repo, &["--here", "只聊天"]);
    assert!(output.status.success() && !String::from_utf8_lossy(&output.stderr).contains("git worktree"), "{}", text(&output));
    assert_eq!(session_files(&repo), 2);
    // attach：接到在跑的那个上
    let output = edith(&home, model.port, &repo, &["attach", "只聊天"]);
    assert!(output.status.success() && String::from_utf8_lossy(&output.stderr).contains("正在跑的大脑上"), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("你好，我是 EDITH。"), "{}", text(&output));
    drop(running);
    let instances = edith(&home, model.port, &repo, &["instances"]);
    assert!(String::from_utf8_lossy(&instances.stdout).contains("没有在跑的大脑"), "{}", text(&instances));
}

#[test]
fn busy_plain_directory_works_alongside() {
    let (work, home) = scratch("plain");
    let model = fake_model(&home.join("requests.jsonl"));
    let _first = start_run(&home, model.port, &work);
    // 不是 git 仓库：开不了 worktree，就在这里一起干（目录不加锁）
    let output = edith(&home, model.port, &work, &["只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("开不了 worktree"), "{}", text(&output));
    let _second = start_run(&home, model.port, &work);
    assert_eq!(registry(&home).len(), 2);
}

#[test]
fn workspace_search_stops_at_git_root() {
    let (work, home) = scratch("gitroot");
    let model = fake_model(&home.join("requests.jsonl"));
    // 外面那层有 .edith，里面是个 git 仓库：在仓库里干活用仓库自己的
    std::fs::create_dir_all(work.join(".edith")).unwrap();
    let repo = work.join("repo");
    repository(&repo);
    let output = edith(&home, model.port, &repo.join("src"), &["只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(session_files(&repo), 1);
    assert_eq!(session_files(&work), 0);
}

#[test]
fn directory_layer_overrides_global_capabilities() {
    let (work, home) = scratch("layers");
    let model = fake_model(&home.join("requests.jsonl"));
    let (a, b) = (work.join("a"), work.join("b"));
    std::fs::create_dir_all(a.join(".edith")).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let global = json!({ "capabilities": [{ "name": "cap", "command": python(), "args": ["-u", fake("cap.py").to_str().unwrap()] }] });
    std::fs::write(home.join("capabilities.json"), global.to_string()).unwrap();
    std::fs::write(
        a.join(".edith").join("capabilities.json"),
        json!({ "capabilities": [{ "name": "cap", "disabled": true }] }).to_string(),
    )
    .unwrap();
    let listed = edith(&home, model.port, &a, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("cap（edith，目录）：关掉了"), "{}", text(&listed));
    let listed = edith(&home, model.port, &b, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("cap（edith，全局） 测试能力"), "{}", text(&listed));
    // 加进目录那层的只在这个目录有
    let limb = fake("limb.py");
    let added = edith(&home, model.port, &b, &["capabilities", "add-mcp", "demo", python(), "-u", limb.to_str().unwrap()]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(std::fs::read_to_string(b.join(".edith").join("capabilities.json")).unwrap().contains("demo"));
    assert!(!std::fs::read_to_string(home.join("capabilities.json")).unwrap().contains("demo"));
}
