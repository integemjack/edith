//! 共享设备（docs/DEVICES.md）：两个目录的大脑经 edith-connect 共用一个设备（tests/fake/device.py）——
//! 只起一个设备程序、设备忙、动作带上是哪个大脑、主动推的交给当前大脑（claim）、没人连着自己退出

mod common;

use std::path::{Path, PathBuf};
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

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 在这个目录跑着的大脑接上了 mic
fn mic_attached(home: &Path, dir: &Path) -> bool {
    std::fs::read_dir(home.join("instances")).into_iter().flatten().flatten().any(|entry| {
        let value: Value = serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap_or_default()).unwrap_or(Value::Null);
        value["workspace"].as_str().map(PathBuf::from).as_deref() == Some(dir)
            && value["capabilities"].as_array().is_some_and(|names| names.contains(&json!("mic")))
    })
}

fn main_session(dir: &Path) -> String {
    let id = std::fs::read_to_string(dir.join(".edith").join("main-session")).unwrap_or_default();
    std::fs::read_to_string(dir.join(".edith").join("sessions").join(format!("{}.json", id.trim()))).unwrap_or_default()
}

#[test]
fn two_brains_share_one_device() {
    let (work, home) = scratch("devices");
    let model = fake_model(&home.join("requests.jsonl"));
    let device_log = home.join("device.jsonl");
    std::fs::write(
        home.join("devices.json"),
        json!({ "devices": [{
            "name": "mic",
            "command": python(),
            "args": ["-u", fake("device.py").to_str().unwrap()],
            "env": { "DEVICE_LOG": device_log.to_str().unwrap(), "DEVICE_ANNOUNCE_DELAY": "2.5" },
            "idleSeconds": 3,
        }]})
        .to_string(),
    )
    .unwrap();
    // 全局能力：所有目录都经中转接 mic
    std::fs::write(
        home.join("capabilities.json"),
        json!({ "capabilities": [{ "name": "mic", "command": env!("CARGO_BIN_EXE_edith-connect"), "args": ["mic"] }] }).to_string(),
    )
    .unwrap();
    let root = std::fs::canonicalize(&work).unwrap();
    let (a, b) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let start = |dir: &Path| {
        Running(
            command(&home, model.port, dir, &["run"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let running_a = start(&a);
    let running_b = start(&b);
    assert!(wait_until(30, || mic_attached(&home, &a) && mic_attached(&home, &b)), "两个大脑没都接上 mic");
    // 只起了一个设备程序
    let initializes = std::fs::read_to_string(&device_log).unwrap().matches("\"initialize\"").count();
    assert_eq!(initializes, 1, "设备程序该只起一个");
    assert!(home.join("devices").join("mic.json").exists());
    let ask = |dir: &Path, words: &str| {
        let output = edith(&home, model.port, dir, &["attach", words]);
        assert!(output.status.success(), "{}", text(&output));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    // 设备忙：a 开始录（holds），b 调设备的动作被拒、说清是谁占着；a 停了 b 就能用
    assert!(ask(&a, "调用工具 mic__record").contains("结果：开始录了"));
    let refused = ask(&b, "调用工具 mic__record");
    assert!(refused.contains("设备忙") && refused.contains(&a.to_string_lossy().to_string()), "{refused}");
    assert!(ask(&a, "调用工具 mic__stop").contains("结果：停了"));
    assert!(ask(&b, "调用工具 mic__record").contains("结果：开始录了"));
    assert!(ask(&b, "调用工具 mic__stop").contains("结果：停了"));
    // 设备程序知道是哪个大脑要的
    let who = ask(&b, "调用工具 mic__who");
    assert!(who.contains(&b.to_string_lossy().to_string()), "{who}");
    // exclusive：a 的哔还没完时 b 调设备被拒
    let beeping = {
        let home = home.clone();
        let a = a.clone();
        let port = model.port;
        std::thread::spawn(move || edith(&home, port, &a, &["attach", "调用工具 mic__beep"]))
    };
    std::thread::sleep(Duration::from_millis(900));
    assert!(ask(&b, "调用工具 mic__who").contains("设备忙"));
    let beeped = beeping.join().unwrap();
    assert!(String::from_utf8_lossy(&beeped.stdout).contains("结果：哔"), "{}", text(&beeped));
    // 当前大脑：a 让设备过一会儿说话，b 马上 claim，设备说的就交给 b
    assert!(ask(&a, "调用工具 mic__announce").contains("一会儿说"));
    assert!(ask(&b, "调用工具 mic__claim").contains("交给这个大脑"));
    assert!(wait_until(15, || main_session(&b).contains("只聊天")), "设备说的没交给 b");
    assert!(wait_until(10, || main_session(&b).contains("你好，我是 EDITH。")));
    assert!(!main_session(&a).contains("只聊天"));
    // 没有大脑连着：设备服务过 idleSeconds 自己退出、删掉登记
    drop(running_a);
    drop(running_b);
    assert!(wait_until(20, || !home.join("devices").join("mic.json").exists()), "设备服务没自己退出");
}
