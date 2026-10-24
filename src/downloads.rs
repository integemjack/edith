//! 共用的工具从 integemjack/edith.tools 下载（edith tools）：Release 里的 index.json 列出每个工具、每个平台的包（地址、SHA-256）。
//! 装在 `~/.edith/tools/<工具>/`（程序在里面），`~/.edith/tools/installed.json` 记装了哪个版本。
//! 能力配置里的命令是个光秃秃的名字时（`edith-connect`），先找装好的工具，再找 PATH；都没有、而 index.json 里有，就当场下载。
//! `EDITH_TOOLS_INDEX` 可以换索引（网址或本地文件；包的地址也可以是本地路径，测试用）。

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::Digest;

use crate::home;

const INDEX: &str = "https://github.com/integemjack/edith.tools/releases/latest/download/index.json";

/// 这个程序编给哪个平台（和 edith.tools 的包名一样）
pub fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("windows", "x86_64") => "windows-x64",
        ("windows", "x86") => "windows-x86",
        ("windows", "aarch64") => "windows-arm64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "x86") => "linux-x86",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "arm") if cfg!(target_feature = "v7") => "linux-armv7",
        ("linux", "arm") => "linux-armv6",
        ("linux", "riscv64") => "linux-riscv64",
        ("linux", "loongarch64") => "linux-loong64",
        ("linux", "powerpc64") => "linux-ppc64le",
        ("linux", "s390x") => "linux-s390x",
        _ => "unknown",
    }
}

fn tools_dir() -> PathBuf {
    home::dir().join("tools")
}

fn executable(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

fn installed_path(name: &str) -> PathBuf {
    tools_dir().join(name).join(executable(name))
}

fn installed() -> Value {
    home::read_json(&tools_dir().join("installed.json")).unwrap_or_else(|| json!({}))
}

/// PATH 里有没有这个程序
fn in_path(command: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false };
    let names: Vec<String> = if cfg!(windows) {
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
        std::iter::once(command.to_string())
            .chain(extensions.split(';').map(|extension| format!("{command}{}", extension.to_lowercase())))
            .collect()
    } else {
        vec![command.to_string()]
    };
    std::env::split_paths(&paths).any(|dir| names.iter().any(|name| dir.join(name).is_file()))
}

/// 能力配置里的命令 → 要起的程序：带路径的原样；名字：装好的工具 → PATH → 从 edith.tools 下载
pub fn resolve(command: &str) -> Result<PathBuf, String> {
    if command.contains('/') || command.contains('\\') {
        return Ok(PathBuf::from(command));
    }
    let tool = installed_path(command);
    if tool.is_file() {
        return Ok(tool);
    }
    if in_path(command) {
        return Ok(PathBuf::from(command));
    }
    // 本机没有：edith.tools 里有就下载
    match index() {
        Ok(index) if index["tools"].get(command).is_some() => {
            eprintln!("\x1b[2m（本机没有 {command}：从 edith.tools 下载）\x1b[0m");
            install_from(&index, command)?;
            Ok(installed_path(command))
        }
        _ => Ok(PathBuf::from(command)),
    }
}

/// 读索引（EDITH_TOOLS_INDEX 换地方）
pub fn index() -> Result<Value, String> {
    let location = std::env::var("EDITH_TOOLS_INDEX").ok().filter(|location| !location.is_empty()).unwrap_or_else(|| INDEX.into());
    let bytes = fetch(&location)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("索引不是 JSON（{location}）：{error}"))
}

/// 网址或本地路径（file:// 也行）的全部内容
fn fetch(location: &str) -> Result<Vec<u8>, String> {
    if !location.starts_with("http://") && !location.starts_with("https://") {
        let path = location.strip_prefix("file://").unwrap_or(location);
        return std::fs::read(path).map_err(|error| format!("读不了 {path}：{error}"));
    }
    let mut response = crate::http::agent().get(location).call().map_err(|error| format!("下载不了 {location}：{error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("下载 {location}：HTTP {status}"));
    }
    let mut bytes = Vec::new();
    response.body_mut().as_reader().read_to_end(&mut bytes).map_err(|error| format!("下载 {location} 断了：{error}"))?;
    Ok(bytes)
}

/// 装（或更新）一个工具：下这个平台的包、核对 SHA-256、解开、换上
pub fn install(name: &str) -> Result<String, String> {
    install_from(&index()?, name)
}

fn install_from(index: &Value, name: &str) -> Result<String, String> {
    let tool = index["tools"].get(name).ok_or(format!("edith.tools 里没有 {name}（edith tools 看有哪些）"))?;
    let asset = tool["assets"].get(platform()).ok_or(format!("{name} 没有这个平台（{}）的包", platform()))?;
    let url = asset["url"].as_str().ok_or("索引里没有包的地址")?;
    let bytes = fetch(url)?;
    let digest: String = sha2::Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    if asset["sha256"].as_str().is_some_and(|wanted| !wanted.eq_ignore_ascii_case(&digest)) {
        return Err(format!("{name} 的包校验不对（下载不完整或被改过）"));
    }
    // 解到临时目录，再换上
    let file = asset["file"].as_str().map(str::to_string).unwrap_or_else(|| url.rsplit('/').next().unwrap_or("tool").to_string());
    let staging = tools_dir().join(format!(".staging-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let archive = staging.join(&file);
    std::fs::write(&archive, &bytes).map_err(|error| error.to_string())?;
    extract(&archive, &staging)?;
    let program = staging.join(executable(name));
    if !program.is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("{file} 里没有 {}", executable(name)));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755));
    }
    let target = tools_dir().join(name);
    std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
    // Windows 上正在跑的程序不能覆盖，先挪开
    let destination = installed_path(name);
    if destination.exists() {
        let old = target.join(format!(".old-{}", std::process::id()));
        let _ = std::fs::rename(&destination, &old);
        let _ = std::fs::remove_file(&old);
    }
    std::fs::rename(&program, &destination).map_err(|error| format!("装不上 {name}：{error}"))?;
    let _ = std::fs::remove_dir_all(&staging);
    let version = index["version"].as_str().unwrap_or("").to_string();
    let mut record = installed();
    record[name] = json!({ "version": version, "platform": platform(), "installed": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false) });
    home::write_json(&tools_dir().join("installed.json"), &record)?;
    Ok(version)
}

/// tar.gz / zip 都交给系统的 tar（macOS、Linux 自带；Windows 10 起的 tar.exe 也认 zip）
fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    let output = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|error| format!("跑不了 tar：{error}"))?;
    if !output.status.success() {
        return Err(format!("解不开 {}：{}", archive.display(), String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(())
}

/// 去掉一个装了的工具
pub fn remove(name: &str) -> bool {
    let removed = std::fs::remove_dir_all(tools_dir().join(name)).is_ok();
    let mut record = installed();
    if let Some(object) = record.as_object_mut()
        && object.remove(name).is_some()
    {
        let _ = home::write_json(&tools_dir().join("installed.json"), &record);
        return true;
    }
    removed
}

/// edith tools：可装的和装了的
pub fn list() -> Result<Vec<(String, String, String, String)>, String> {
    let record = installed();
    let index = index();
    let mut rows = Vec::new();
    if let Ok(index) = &index {
        for (name, tool) in index["tools"].as_object().into_iter().flatten() {
            let available = if tool["assets"].get(platform()).is_some() {
                index["version"].as_str().unwrap_or("").to_string()
            } else {
                "（这个平台没有）".into()
            };
            let current = record[name]["version"].as_str().unwrap_or("").to_string();
            rows.push((name.clone(), current, available, tool["description"].as_str().unwrap_or("").to_string()));
        }
    }
    for (name, entry) in record.as_object().into_iter().flatten() {
        if !rows.iter().any(|row| &row.0 == name) {
            rows.push((name.clone(), entry["version"].as_str().unwrap_or("").to_string(), String::new(), String::new()));
        }
    }
    if rows.is_empty()
        && let Err(error) = index
    {
        return Err(error);
    }
    Ok(rows)
}
