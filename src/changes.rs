//! 会话改动（同 Friday 的 SessionChanges，存的格式一样，Friday 仓库 qt/docs/FILES.md §5）：这段会话（含它派的子智能体）
//! 第一次动一个文件之前的样子是基线；列出净改动（改了又改回去的不算）、看改动（基线对现在）、撤销（改回基线，新建的删掉）。
//!
//! 基线在 `<项目数据目录>/changes/<会话 id>/`：`index.json` = `{next, files:[{path, existed, blob?}]}`，`blobs/<n>` 是原文。
//! - write_file / edit_file 写之前记（tools.rs）；
//! - run_shell 执行前后各看一遍（friday-core 的 file_changes）：新建的记「原来没有」，改的、删的拿执行前的行指纹到 git 里找
//!   `HEAD:./<名>`、`:./<名>`（暂存区）对得上的那一版，找不到只知道改过（known = false，撤不了）。
//!
//! 超过 1 MB 的不存原文；一段会话最多 1000 个文件；一次命令最多到 git 里查 20 个。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use friday_core::file_changes::{Changed, Kind};
use friday_core::line_diff;
use serde_json::{Value, json};

use crate::home;

const MAX_BLOB: u64 = 1 << 20;
const MAX_FILES: usize = 1000;
const MAX_GIT_LOOKUPS: usize = 20;

/// 改基线的一次一个（几个子智能体同时写）
static LOCK: Mutex<()> = Mutex::new(());

/// 这个项目的基线放在哪（测试里换成临时目录）
fn base() -> PathBuf {
    home::project_dir().join("changes")
}

/// 一段会话的基线目录
#[derive(Clone)]
struct Store(PathBuf);

impl Store {
    fn of(session: &str) -> Store {
        Store(base().join(session.to_lowercase()))
    }

    fn read(&self) -> Value {
        home::read_json(&self.0.join("index.json")).unwrap_or_else(|| json!({ "next": 1, "files": [] }))
    }

    fn write(&self, index: &Value) {
        let _ = std::fs::create_dir_all(&self.0);
        let _ = home::write_json(&self.0.join("index.json"), index);
    }

    fn blob(&self, n: u64) -> PathBuf {
        self.0.join("blobs").join(n.to_string())
    }
}

fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b }
}

fn entry<'a>(index: &'a Value, path: &str) -> Option<&'a Value> {
    index["files"].as_array()?.iter().find(|file| file["path"].as_str().is_some_and(|known| same_path(known, path)))
}

/// 记一个文件的基线（已经记过的不动：基线是第一次动它之前的样子）。content：原文（None：原来没有；Some(None)：有，但不知道内容）
fn note(store: &Store, path: &str, content: Option<Option<Vec<u8>>>) {
    let mut index = store.read();
    if entry(&index, path).is_some() || index["files"].as_array().map_or(0, Vec::len) >= MAX_FILES {
        return;
    }
    let mut file = json!({ "path": path, "existed": content.is_some() });
    if let Some(Some(bytes)) = content
        && (bytes.len() as u64) <= MAX_BLOB
    {
        let next = index["next"].as_u64().unwrap_or(1);
        let _ = std::fs::create_dir_all(store.0.join("blobs"));
        if std::fs::write(store.blob(next), &bytes).is_ok() {
            file["blob"] = json!(next);
            index["next"] = json!(next + 1);
        }
    }
    if !index["files"].is_array() {
        index["files"] = json!([]);
    }
    index["files"].as_array_mut().unwrap().push(file);
    store.write(&index);
}

/// write_file / edit_file 写之前（path：绝对路径，/ 分隔）
pub fn before_write(session: &str, path: &str) {
    if !session.is_empty() {
        note_write(&Store::of(session), path);
    }
}

fn note_write(store: &Store, path: &str) {
    let _guard = LOCK.lock().unwrap();
    let target = Path::new(path);
    let content = if target.is_file() {
        let size = std::fs::metadata(target).map(|meta| meta.len()).unwrap_or(0);
        Some(if size <= MAX_BLOB { std::fs::read(target).ok() } else { None })
    } else {
        None
    };
    note(store, path, content);
}

/// git 仓库里这个文件 HEAD / 暂存区里每行指纹对得上 before 的那一版
fn from_git(path: &str, before: &[u64]) -> Option<Vec<u8>> {
    let file = Path::new(path);
    let mut dir = file.parent()?;
    let repo = loop {
        if dir.join(".git").exists() {
            break dir;
        }
        dir = dir.parent()?;
    };
    let relative = file.strip_prefix(repo).ok()?.to_string_lossy().replace('\\', "/");
    for spec in [format!("HEAD:./{relative}"), format!(":./{relative}")] {
        let output = std::process::Command::new("git").arg("-C").arg(repo).args(["show", &spec]).output().ok()?;
        if output.status.success()
            && let Ok(text) = std::str::from_utf8(&output.stdout)
            && line_diff::line_hashes(text) == before
        {
            return Some(output.stdout);
        }
    }
    None
}

/// run_shell 跑完：命令动了的文件记基线
pub fn note_shell(session: &str, changed: &[Changed]) {
    if !session.is_empty() && !changed.is_empty() {
        note_changed(&Store::of(session), changed);
    }
}

fn note_changed(store: &Store, changed: &[Changed]) {
    let _guard = LOCK.lock().unwrap();
    let index = store.read();
    let mut lookups = 0;
    for file in changed {
        if entry(&index, &file.path).is_some() {
            continue;
        }
        let content = match file.kind {
            Kind::Created => None,
            Kind::Modified | Kind::Deleted => {
                let found = match &file.before {
                    Some(before) if lookups < MAX_GIT_LOOKUPS => {
                        lookups += 1;
                        from_git(&file.path, before)
                    }
                    _ => None,
                };
                Some(found)
            }
        };
        note(store, &file.path, content);
    }
}

/// 基线：(原来有没有, 原文——不知道的是 None)
fn baseline(store: &Store, file: &Value) -> (bool, Option<Vec<u8>>) {
    let existed = file["existed"].as_bool().unwrap_or(true);
    let blob = file["blob"].as_u64().and_then(|blob| std::fs::read(store.blob(blob)).ok());
    (existed, if existed { blob } else { Some(Vec::new()) })
}

fn current(path: &str) -> Option<Vec<u8>> {
    let target = Path::new(path);
    target.is_file().then(|| std::fs::read(target).ok()).flatten()
}

fn text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok().filter(|text| !text.contains('\0'))
}

/// 这段会话改了哪些文件（净改动）：[{path, status A / M / D, known, text, added, removed}]，不知道的增删是 -1
pub fn list(session: &str) -> Value {
    list_in(&Store::of(session))
}

fn list_in(store: &Store) -> Value {
    let index = store.read();
    let mut files = Vec::new();
    for file in index["files"].as_array().into_iter().flatten() {
        let path = file["path"].as_str().unwrap_or("");
        let (existed, old) = baseline(store, file);
        let now = current(path);
        let status = match (existed, now.is_some()) {
            (false, false) => continue,
            (false, true) => "A",
            (true, false) => "D",
            (true, true) => "M",
        };
        // 改了又改回去的不算
        if status == "M"
            && let (Some(old), Some(now)) = (&old, &now)
            && old == now
        {
            continue;
        }
        let known = old.is_some();
        let old_text = old.as_deref().map(text);
        let new_text = now.as_deref().map(text);
        let is_text = old_text.is_none_or(|text| text.is_some()) && new_text.is_none_or(|text| text.is_some());
        let (added, removed) = match (known, is_text) {
            (true, true) => {
                let counts = line_diff::count_text(old_text.flatten().unwrap_or(""), new_text.flatten().unwrap_or(""));
                (counts.added as i64, counts.removed as i64)
            }
            _ => (-1, -1),
        };
        files.push(json!({ "path": path, "status": status, "known": known, "text": is_text, "added": added, "removed": removed }));
    }
    json!({ "files": files })
}

/// 一个文件的改动：基线和现在的全文（各最多 1 MB），逐行对比由界面做
pub fn diff(session: &str, path: &str) -> Result<Value, String> {
    diff_in(&Store::of(session), path)
}

fn diff_in(store: &Store, path: &str) -> Result<Value, String> {
    let index = store.read();
    let file = entry(&index, path).ok_or_else(|| format!("这段会话没动过 {path}"))?;
    let (existed, old) = baseline(store, file);
    let now = current(path);
    let status = match (existed, now.is_some()) {
        (false, _) => "A",
        (true, false) => "D",
        (true, true) => "M",
    };
    let too_large =
        old.as_ref().is_some_and(|bytes| bytes.len() as u64 > MAX_BLOB) || now.as_ref().is_some_and(|bytes| bytes.len() as u64 > MAX_BLOB);
    let old_text = old.as_deref().and_then(text);
    let new_text = now.as_deref().and_then(text);
    let binary = (old.is_some() && old_text.is_none()) || (now.is_some() && new_text.is_none());
    let mut result = json!({ "path": path, "status": status, "known": old.is_some(), "binary": binary, "tooLarge": too_large });
    if !binary && !too_large {
        result["old"] = json!(old_text.unwrap_or(""));
        result["new"] = json!(new_text.unwrap_or(""));
    }
    Ok(result)
}

/// 撤销：改回基线（这段会话新建的删掉）。path：只撤这一个，不给撤全部；撤不了的（不知道原文）列在 failed 里
pub fn revert(session: &str, path: Option<&str>) -> Value {
    revert_in(&Store::of(session), path)
}

fn revert_in(store: &Store, path: Option<&str>) -> Value {
    let _guard = LOCK.lock().unwrap();
    let index = store.read();
    let mut failed = Vec::new();
    for file in index["files"].as_array().into_iter().flatten() {
        let target = file["path"].as_str().unwrap_or("");
        if path.is_some_and(|wanted| !same_path(wanted, target)) {
            continue;
        }
        let (existed, old) = baseline(store, file);
        let outcome = match (existed, old) {
            (false, _) => match std::fs::remove_file(target) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
                _ => Ok(()),
            },
            (true, Some(bytes)) => Path::new(target)
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::write(target, bytes))
                .map_err(|error| error.to_string()),
            (true, None) => Err("不知道原来的样子（命令改的、git 里没找到，或超过 1 MB）".into()),
        };
        if let Err(error) = outcome {
            failed.push(json!({ "path": target, "error": error }));
        }
    }
    if let Some(wanted) = path
        && entry(&index, wanted).is_none()
    {
        failed.push(json!({ "path": wanted, "error": "这段会话没动过它" }));
    }
    json!({ "failed": failed })
}

/// 删会话时：基线不要了
pub fn remove(session: &str) {
    let _ = std::fs::remove_dir_all(&Store::of(session).0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_lists_and_reverts() {
        let home = std::env::temp_dir().join(format!("edith-changes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = Store(home.join("changes").join("s1"));
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let slash = |name: &str| work.join(name).to_string_lossy().replace('\\', "/");
        std::fs::write(work.join("a.txt"), "1\n2\n").unwrap();
        // 改 a、新建 b；改两次基线还是第一次之前的
        note_write(&store, &slash("a.txt"));
        std::fs::write(work.join("a.txt"), "1\n3\n4\n").unwrap();
        note_write(&store, &slash("a.txt"));
        std::fs::write(work.join("a.txt"), "1\n3\n4\n5\n").unwrap();
        note_write(&store, &slash("b.txt"));
        std::fs::write(work.join("b.txt"), "x\n").unwrap();
        // 命令删了 c（不在 git 里：只知道删了）
        note_changed(&store, &[Changed { path: slash("c.txt"), kind: Kind::Deleted, before: Some(vec![1]) }]);
        let files = list_in(&store)["files"].as_array().unwrap().clone();
        let find = |name: &str| files.iter().find(|file| file["path"].as_str().unwrap().ends_with(name)).cloned().unwrap();
        assert_eq!(
            (find("a.txt")["status"].as_str(), find("a.txt")["added"].as_i64(), find("a.txt")["removed"].as_i64()),
            (Some("M"), Some(3), Some(1))
        );
        assert_eq!((find("b.txt")["status"].as_str(), find("b.txt")["added"].as_i64()), (Some("A"), Some(1)));
        assert_eq!((find("c.txt")["status"].as_str(), find("c.txt")["known"].as_bool()), (Some("D"), Some(false)));
        let shown = diff_in(&store, &slash("a.txt")).unwrap();
        assert_eq!((shown["old"].as_str(), shown["new"].as_str()), (Some("1\n2\n"), Some("1\n3\n4\n5\n")));
        // 撤销全部：a 改回去、b 删掉、c 撤不了
        let reverted = revert_in(&store, None);
        assert_eq!(std::fs::read_to_string(work.join("a.txt")).unwrap(), "1\n2\n");
        assert!(!work.join("b.txt").exists());
        assert_eq!(reverted["failed"].as_array().unwrap().len(), 1);
        // 改回去了：净改动里没有了（只剩撤不了的 c）
        assert_eq!(list_in(&store)["files"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }
}
