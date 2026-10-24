//! 一个目录一个大脑（同 Claude Code）：会话在 ~/.edith/projects/<路径>/、-c 接着最近的、-r 接着指定的、
//! -w 开 git worktree（没改东西退出时清掉）、目录不加锁可以开几个、三层能力配置、先读后改、在跑的大脑的登记

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{command, edith, fake, fake_model, python, scratch, session_files, text};
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

fn registry(home: &Path) -> Vec<Value> {
    std::fs::read_dir(home.join("instances"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| serde_json::from_str(&std::fs::read_to_string(entry.path()).ok()?).ok())
        .collect()
}

/// 这个目录唯一的会话（id、历史里用户说了几句）
fn only_session(home: &Path, workspace: &Path) -> (String, usize) {
    let session = common::read_sessions(home, workspace).remove(0);
    let users = session["history"].as_array().unwrap().iter().filter(|message| message["role"] == "user").count();
    (session["id"].as_str().unwrap().to_string(), users)
}

#[test]
fn sessions_live_outside_the_project() {
    let (work, home) = scratch("dirs");
    let model = fake_model(&home.join("requests.jsonl"));
    let (a, b) = (work.join("a"), work.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    for dir in [&a, &b] {
        let output = edith(&home, model.port, dir, &["只聊天"]);
        assert!(output.status.success(), "{}", text(&output));
    }
    assert_eq!(session_files(&home, &a), 1);
    assert_eq!(session_files(&home, &b), 1);
    // 项目目录里什么都不放
    assert!(!a.join(".edith").exists() && !b.join(".edith").exists());
    // -c：接着最近的那个；-r：接着指定的
    let output = edith(&home, model.port, &a, &["-c", "只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    let (id, users) = only_session(&home, &a);
    assert_eq!(users, 2);
    let output = edith(&home, model.port, &a, &["-r", &id[..8], "只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(only_session(&home, &a), (id, 3));
    // 不带 -c 就是新会话
    let output = edith(&home, model.port, &a, &["只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(session_files(&home, &a), 2);
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
    let before: Vec<Value> = registry(home);
    let log = home.join(format!("run-{}.log", before.len()));
    let stderr = std::fs::File::create(&log).unwrap();
    let mut child = command(home, port, dir, &["run"]).stdout(std::process::Stdio::null()).stderr(stderr).spawn().unwrap();
    let pid = child.id();
    // 认它自己的那份登记（进程号），不按条数：别的登记可能同时被清掉
    let appeared = wait_until(20, || registry(home).iter().any(|entry| entry["pid"] == pid));
    if !appeared {
        let exited = child.try_wait().ok().flatten();
        let _ = child.kill();
        panic!(
            "登记没出现（进程 {pid}，退出：{exited:?}）\n之前的登记：{before:?}\n现在的登记：{:?}\n它的输出：{}",
            registry(home),
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
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
fn worktree_only_when_asked_and_cleaned_when_unchanged() {
    let (work, home) = scratch("worktree");
    let model = fake_model(&home.join("requests.jsonl"));
    let repo = std::fs::canonicalize(&work).unwrap();
    repository(&repo);
    // 有大脑在跑也不自动开 worktree：一起在这里干（目录不加锁）
    let running = start_run(&home, model.port, &repo);
    let output = edith(&home, model.port, &repo, &["只聊天"]);
    assert!(output.status.success() && !String::from_utf8_lossy(&output.stderr).contains("git worktree"), "{}", text(&output));
    // -w：开一个；没改东西，退出时清掉
    let output = edith(&home, model.port, &repo.join("src"), &["-w", "只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("git worktree") && stderr.contains("清掉了"), "{}", text(&output));
    assert!(!git(&repo, &["worktree", "list"]).contains("edith/"));
    assert!(!git(&repo, &["branch"]).contains("edith/"));
    // 在子目录里起的对应到 worktree 的同一个子目录
    let requests = std::fs::read_to_string(home.join("requests.jsonl")).unwrap();
    let worktrees = repo.join(".edith").join("worktrees");
    assert!(requests.contains(&worktrees.to_string_lossy().replace('\\', "\\\\")), "工作目录该在 worktree 里");
    // 改了东西：留着，告诉怎么合并
    let output = edith(&home, model.port, &repo, &["-w", "写个 hello.txt 然后核对"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("改动在 worktree"), "{}", text(&output));
    let trees: Vec<_> = std::fs::read_dir(&worktrees).unwrap().flatten().map(|entry| entry.path()).collect();
    assert_eq!(trees.len(), 1);
    assert!(trees[0].join("hello.txt").exists() && !repo.join("hello.txt").exists());
    assert!(git(&repo, &["worktree", "list"]).contains("edith/"));
    // .edith/worktrees 不进 git
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    // worktree 里的会话记在仓库名下：worktree 删了也找得到（run 的、在这里聊的、-w 的两个）
    assert_eq!(session_files(&home, &repo), 4);
    // 被强行结束留下的 worktree：edith worktree 看得到，prune 清掉没改东西的，改了的留着
    let leftover = worktrees.join("left00");
    git(&repo, &["worktree", "add", "-q", "-b", "edith/left00", &leftover.to_string_lossy(), "HEAD"]);
    let listed = edith(&home, model.port, &repo, &["worktree"]);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("left00  edith/left00  没改东西") && stdout.contains("有没提交的改动"), "{}", text(&listed));
    let pruned = edith(&home, model.port, &repo, &["worktree", "prune"]);
    assert!(String::from_utf8_lossy(&pruned.stdout).contains("清掉了 1 个"), "{}", text(&pruned));
    assert!(!leftover.exists() && trees[0].exists());
    assert!(!git(&repo, &["branch"]).contains("edith/left00"));
    // attach：接到在跑的那个上
    let output = edith(&home, model.port, &repo, &["attach", "只聊天"]);
    assert!(output.status.success() && String::from_utf8_lossy(&output.stderr).contains("正在跑的大脑上"), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("你好，我是 EDITH。"), "{}", text(&output));
    // 同一个目录可以再开一个
    let second = start_run(&home, model.port, &repo);
    assert_eq!(registry(&home).len(), 2);
    drop(running);
    drop(second);
    let instances = edith(&home, model.port, &repo, &["instances"]);
    assert!(String::from_utf8_lossy(&instances.stdout).contains("没有在跑的大脑"), "{}", text(&instances));
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
    assert_eq!(session_files(&home, &repo), 1);
    assert_eq!(session_files(&home, &work), 0);
}

#[test]
fn three_config_layers() {
    let (work, home) = scratch("layers");
    let model = fake_model(&home.join("requests.jsonl"));
    let (a, b) = (work.join("a"), work.join("b"));
    std::fs::create_dir_all(a.join(".edith")).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let global = json!({ "capabilities": [{ "name": "cap", "command": python(), "args": ["-u", fake("cap.py").to_str().unwrap()] }] });
    std::fs::write(home.join("capabilities.json"), global.to_string()).unwrap();
    // 项目那层关掉全局的
    std::fs::write(
        a.join(".edith").join("capabilities.json"),
        json!({ "capabilities": [{ "name": "cap", "disabled": true }] }).to_string(),
    )
    .unwrap();
    // 项目那层的配置要先信任
    assert!(edith(&home, model.port, &a, &["trust", "--yes"]).status.success());
    let listed = edith(&home, model.port, &a, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("cap（项目）：关掉了"), "{}", text(&listed));
    // 本地那层再打开
    std::fs::write(
        a.join(".edith").join("capabilities.local.json"),
        json!({ "capabilities": [{ "name": "cap", "disabled": false }] }).to_string(),
    )
    .unwrap();
    assert!(edith(&home, model.port, &a, &["trust", "--yes"]).status.success());
    let listed = edith(&home, model.port, &a, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("cap（本地） 测试能力"), "{}", text(&listed));
    let listed = edith(&home, model.port, &b, &["capabilities"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("cap（全局） 测试能力"), "{}", text(&listed));
    // 默认加进本地那层；--project 加进项目那层（提交进仓库的）
    let cap = fake("cap.py");
    let added = edith(&home, model.port, &b, &["capabilities", "add", "demo", python(), "-u", cap.to_str().unwrap()]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(std::fs::read_to_string(b.join(".edith").join("capabilities.local.json")).unwrap().contains("demo"));
    let added = edith(&home, model.port, &b, &["--project", "capabilities", "add", "shared", python(), "-u", cap.to_str().unwrap()]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(std::fs::read_to_string(b.join(".edith").join("capabilities.json")).unwrap().contains("shared"));
    assert!(!std::fs::read_to_string(home.join("capabilities.json")).unwrap().contains("demo"));
}

#[test]
fn edits_need_a_fresh_read() {
    let (work, home) = scratch("readfirst");
    let model = fake_model(&home.join("requests.jsonl"));
    std::fs::write(work.join("notes.txt"), "原来的\n").unwrap();
    // 没读过就覆盖：拒绝
    let write = r#"调用工具 write_file 参数 {"path":"notes.txt","content":"新的\n"}"#;
    let output = edith(&home, model.port, &work, &[write]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("还没读过"), "{}", text(&output));
    assert_eq!(std::fs::read_to_string(work.join("notes.txt")).unwrap(), "原来的\n");
    // 同一个会话里读过再改：行
    let output = edith(&home, model.port, &work, &["-c", r#"调用工具 read_file 参数 {"path":"notes.txt"}"#]);
    assert!(output.status.success(), "{}", text(&output));
    let output = edith(&home, model.port, &work, &["-c", write]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("还没读过"), "{}", text(&output));
    assert_eq!(std::fs::read_to_string(work.join("notes.txt")).unwrap(), "新的\n");
    // 读过之后别人改了：要重读
    std::fs::write(work.join("notes.txt"), "别人改的\n").unwrap();
    let output = edith(&home, model.port, &work, &["-c", write]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("被改过"), "{}", text(&output));
    assert_eq!(std::fs::read_to_string(work.join("notes.txt")).unwrap(), "别人改的\n");
}

#[test]
fn project_config_needs_trust() {
    let (work, home) = scratch("trust");
    let model = fake_model(&home.join("requests.jsonl"));
    std::fs::create_dir_all(work.join(".edith")).unwrap();
    let marker = home.join("ran.txt");
    // 别人的仓库里写的能力：一跑就会起
    let config = |text: &str| {
        // 写个记号就退出（退出码不是 0：大脑不用等它连进来）
        let script = format!("import sys; open({:?}, 'a').write({text:?}); sys.exit(3)", marker.to_str().unwrap());
        json!({ "capabilities": [{ "name": "evil", "command": python(), "args": ["-c", script] }] }).to_string()
    };
    std::fs::write(work.join(".edith").join("capabilities.json"), config("1")).unwrap();
    // 没信任：不起，告诉怎么信任
    let output = edith(&home, model.port, &work, &["只聊天"]);
    assert!(output.status.success() && String::from_utf8_lossy(&output.stderr).contains("edith trust"), "{}", text(&output));
    assert!(!marker.exists(), "没信任就起了");
    let shown = edith(&home, model.port, &work, &["trust"]);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("能力 evil："), "{}", text(&shown));
    // 信任了才起
    assert!(edith(&home, model.port, &work, &["trust", "--yes"]).status.success());
    let _ = edith(&home, model.port, &work, &["只聊天"]);
    assert!(marker.exists(), "信任了没起");
    // 内容变了（git pull 拉来新命令）：又要信任
    std::fs::remove_file(&marker).unwrap();
    std::fs::write(work.join(".edith").join("capabilities.json"), config("2")).unwrap();
    let _ = edith(&home, model.port, &work, &["只聊天"]);
    assert!(!marker.exists(), "改过的配置没信任就起了");
}

#[test]
fn risky_tools_need_approval() {
    let (work, home) = scratch("approval");
    let model = fake_model(&home.join("requests.jsonl"));
    std::fs::write(work.join(".env"), "KEY=secret").unwrap();
    std::fs::write(work.join("notes.txt"), "hi").unwrap();
    // 不在终端里、没有能批准的能力、没 --auto：要批准的都不做
    let ask = |task: &str| command(&home, model.port, &work, &[task]).env_remove("EDITH_PERMISSION_MODE").output().unwrap();
    let output = ask(r#"调用工具 write_file 参数 {"path":"x.txt","content":"x"}"#);
    assert!(String::from_utf8_lossy(&output.stdout).contains("没人能批"), "{}", text(&output));
    assert!(!work.join("x.txt").exists());
    let output = ask(r#"调用工具 read_file 参数 {"path":".env"}"#);
    assert!(String::from_utf8_lossy(&output.stdout).contains("读敏感的"), "{}", text(&output));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"), "{}", text(&output));
    // 工作目录里普通的读不用问
    let output = ask(r#"调用工具 list_directory 参数 {"path":"."}"#);
    assert!(String::from_utf8_lossy(&output.stdout).contains("notes.txt"), "{}", text(&output));
    // --auto：不问
    let output = command(&home, model.port, &work, &["--auto", r#"调用工具 write_file 参数 {"path":"x.txt","content":"x"}"#])
        .env_remove("EDITH_PERMISSION_MODE")
        .output()
        .unwrap();
    assert!(output.status.success() && work.join("x.txt").exists(), "{}", text(&output));
    // config.json 的 permissions.deny：--auto 也挡
    std::fs::write(home.join("config.json"), json!({ "permissions": { "deny": ["run_shell(rm *)"] } }).to_string()).unwrap();
    let output = command(&home, model.port, &work, &[r#"调用工具 run_shell 参数 {"command":"rm x.txt"}"#]).output().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("permissions.deny"), "{}", text(&output));
    assert!(work.join("x.txt").exists());
}

#[test]
fn upgrades_old_data() {
    let (work, home) = scratch("upgrade");
    let model = fake_model(&home.join("requests.jsonl"));
    // v0.1.9 的样子：会话在项目里、主会话、经 edith-mcp 接的 MCP 服务
    let old = work.join(".edith").join("sessions");
    std::fs::create_dir_all(&old).unwrap();
    let id = "1234abcd-0000-0000-0000-000000000000";
    let session =
        json!({ "id": id, "title": "老会话", "cwd": work.to_str().unwrap(), "history": [{ "role": "user", "content": "以前说的" }] });
    std::fs::write(old.join(format!("{id}.json")), session.to_string()).unwrap();
    std::fs::write(work.join(".edith").join("main-session"), id).unwrap();
    let capabilities = json!({ "capabilities": [
        { "name": "fs", "command": "edith-mcp", "args": [python(), "-u", fake("limb.py").to_str().unwrap()] },
        { "name": "keep", "command": "something-else" },
    ]});
    std::fs::write(home.join("capabilities.json"), capabilities.to_string()).unwrap();
    let listed = edith(&home, model.port, &work, &["sessions"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("老会话"), "{}", text(&listed));
    assert!(String::from_utf8_lossy(&listed.stderr).contains("升级"), "{}", text(&listed));
    assert!(!old.exists() && !work.join(".edith").join("main-session").exists());
    let output = edith(&home, model.port, &work, &["-r", "1234abcd", "只聊天"]);
    assert!(output.status.success(), "{}", text(&output));
    let history = common::read_sessions(&home, &work).remove(0)["history"].to_string();
    assert!(history.contains("以前说的") && history.contains("我是 EDITH"), "{history}");
    // MCP 挪进 mcp.json，别的能力留着
    let mcp = std::fs::read_to_string(home.join("mcp.json")).unwrap();
    assert!(mcp.contains("limb.py") && !mcp.contains("edith-mcp"), "{mcp}");
    let left = std::fs::read_to_string(home.join("capabilities.json")).unwrap();
    assert!(left.contains("something-else") && !left.contains("edith-mcp"), "{left}");
    let tools = edith(&home, model.port, &work, &["mcp"]);
    assert!(String::from_utf8_lossy(&tools.stdout).contains("工具：echo"), "{}", text(&tools));
}
