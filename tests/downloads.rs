//! edith tools：从（本地的假）edith.tools 索引下载工具——核对 SHA-256、解开、装上；能力要用的程序本机没有就自己下

mod common;

use std::path::Path;
use std::process::Command;

use common::{edith, fake_model, scratch, text};
use serde_json::{Value, json};
use sha2::Digest;

const PLATFORMS: &[&str] = &[
    "macos-arm64",
    "macos-x64",
    "windows-x64",
    "windows-x86",
    "windows-arm64",
    "linux-x64",
    "linux-x86",
    "linux-arm64",
    "linux-armv7",
    "linux-armv6",
    "linux-riscv64",
    "linux-loong64",
    "linux-ppc64le",
    "linux-s390x",
];

/// 假的 edith.tools：一个工具 fake-tool（其实就是 edith 这个程序改个名），每个平台都指向同一个包
fn fake_index(root: &Path, corrupt: bool) -> std::path::PathBuf {
    let staging = root.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let exe = if cfg!(windows) { "fake-tool.exe" } else { "fake-tool" };
    std::fs::copy(env!("CARGO_BIN_EXE_edith"), staging.join(exe)).unwrap();
    let archive = root.join("fake-tool-v9.9.9.tar.gz");
    let status = Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&staging).arg(exe).status().unwrap();
    assert!(status.success());
    let digest: String = sha2::Sha256::digest(std::fs::read(&archive).unwrap()).iter().map(|byte| format!("{byte:02x}")).collect();
    let asset = json!({ "file": "fake-tool-v9.9.9.tar.gz", "url": archive.to_string_lossy(), "sha256": if corrupt { "00".repeat(32) } else { digest } });
    let assets: serde_json::Map<String, Value> = PLATFORMS.iter().map(|platform| (platform.to_string(), asset.clone())).collect();
    let index = root.join(if corrupt { "bad-index.json" } else { "index.json" });
    std::fs::write(
        &index,
        // 各个工具各自发版：顶上的 version 是索引生成的时间，工具的版本在它自己下面
        json!({ "version": "2026-10-24T00:00:00Z", "tools": { "fake-tool": { "description": "假的工具", "version": "v9.9.9", "assets": assets } } })
            .to_string(),
    )
    .unwrap();
    index
}

fn with_index(home: &Path, port: u16, cwd: &Path, index: &Path, args: &[&str]) -> std::process::Output {
    common::command(home, port, cwd, args).env("EDITH_TOOLS_INDEX", index).output().unwrap()
}

#[test]
fn installs_checks_and_resolves_tools() {
    let (work, home) = scratch("downloads");
    let model = fake_model(&home.join("requests.jsonl"));
    let index = fake_index(&home, false);
    let listed = with_index(&home, model.port, &work, &index, &["tools"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("fake-tool  没装，可装 v9.9.9"), "{}", text(&listed));
    let installed = with_index(&home, model.port, &work, &index, &["tools", "install", "fake-tool"]);
    assert!(
        installed.status.success() && String::from_utf8_lossy(&installed.stdout).contains("装好了 fake-tool v9.9.9"),
        "{}",
        text(&installed)
    );
    let program = home.join("tools").join("fake-tool").join(if cfg!(windows) { "fake-tool.exe" } else { "fake-tool" });
    let version = Command::new(&program).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("edith "), "装上的程序跑不起来");
    let listed = with_index(&home, model.port, &work, &index, &["tools"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("fake-tool  装了 v9.9.9"), "{}", text(&listed));
    // 校验不对：不装
    let bad = fake_index(&home.join("bad"), true);
    let refused = with_index(&home, model.port, &work, &bad, &["tools", "install", "fake-tool"]);
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("校验不对"), "{}", text(&refused));
    assert!(with_index(&home, model.port, &work, &index, &["tools", "remove", "fake-tool"]).status.success());
    assert!(!program.exists());
    // 能力要用 fake-tool、本机没有：自己下（它连不进来没关系，这里只看下没下）
    let output = with_index(&home, model.port, &work, &index, &["capabilities", "add", "x", "fake-tool", "--nope"]);
    assert!(!output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("从 edith.tools 下载"), "{}", text(&output));
    assert!(program.exists(), "没自己下");
    let _ = edith;
}
