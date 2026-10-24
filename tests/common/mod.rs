//! 端到端测试共用：假模型、单独的目录、跑 edith
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

pub fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

pub fn fake(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fake").join(name)
}

/// 起假模型，读它挑的端口；测试结束时结束它
pub struct FakeModel {
    child: Child,
    pub port: u16,
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

pub fn fake_model(log: &Path) -> FakeModel {
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
pub fn scratch(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("edith-smoke-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (work, home) = (root.join("work"), root.join("home"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    (work, home)
}

/// edith 命令（指向假模型、单独的 EDITH_HOME）
pub fn command(home: &Path, port: u16, cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_edith"));
    command
        .args(args)
        .current_dir(cwd)
        .env("EDITH_HOME", home)
        .env("EDITH_BASE_URL", format!("http://127.0.0.1:{port}/v1"))
        .env("EDITH_MODEL", "fake")
        .env("EDITH_API_KEY", "none")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(Stdio::null());
    command
}

pub fn edith(home: &Path, port: u16, cwd: &Path, args: &[&str]) -> Output {
    command(home, port, cwd, args).output().unwrap()
}

/// 这个工作目录的数据在 EDITH_HOME 下的哪（同 home::project_dir：路径里非字母数字换成 -）
pub fn project_dir(home: &Path, workspace: &Path) -> PathBuf {
    let path = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf()).to_string_lossy().into_owned();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path).to_string();
    let encoded: String = path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    home.join("projects").join(encoded)
}

/// 这个工作目录有几个会话文件
pub fn session_files(home: &Path, workspace: &Path) -> usize {
    std::fs::read_dir(project_dir(home, workspace).join("sessions")).map(|entries| entries.count()).unwrap_or(0)
}

pub fn text(output: &Output) -> String {
    format!("{}\n--- stderr ---\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}
