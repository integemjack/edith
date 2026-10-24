//! 目录里已经有大脑在干活时，新起的大脑开一个 git worktree 去那里干（互不干扰）：
//! `<仓库根>/.edith/worktrees/<编号>`，分支 `edith/<编号>`，从 HEAD 开（别的大脑没提交的改动不带过去）。
//! `.edith/` 写进仓库的 info/exclude，git 不当它是要提交的东西。干完 worktree 和分支留着，由用户合并或删掉。

use std::path::{Path, PathBuf};
use std::process::Command;

/// Windows 上 canonicalize 出来的路径带 \\?\ 前缀，git 不认：去掉（UNC 的 \\?\UNC\ 留着）
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

fn git(directory: &Path, arguments: &[&str]) -> Result<String, String> {
    let output =
        Command::new("git").arg("-C").arg(plain(directory)).args(arguments).output().map_err(|error| format!("跑不了 git：{error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 开好的 worktree
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

/// 在 workspace 所在的仓库开一个 worktree；不是 git 仓库（或没有提交）返回原因
pub fn create(workspace: &Path) -> Result<Worktree, String> {
    let top = PathBuf::from(git(workspace, &["rev-parse", "--show-toplevel"]).map_err(|_| "不是 git 仓库".to_string())?);
    git(&top, &["rev-parse", "--verify", "HEAD"]).map_err(|_| "仓库还没有提交，开不了 worktree".to_string())?;
    // .edith/ 不进 git
    let common = PathBuf::from(git(&top, &["rev-parse", "--git-common-dir"])?);
    let common = if common.is_absolute() { common } else { top.join(common) };
    let exclude = common.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !current.lines().any(|line| line.trim() == ".edith/") {
        let _ = std::fs::create_dir_all(common.join("info"));
        let separator = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        let _ = std::fs::write(&exclude, format!("{current}{separator}.edith/\n"));
    }
    let name = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
    let path = top.join(".edith").join("worktrees").join(&name);
    let branch = format!("edith/{name}");
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|error| error.to_string())?;
    git(&top, &["worktree", "add", "-q", "-b", &branch, &path.to_string_lossy(), "HEAD"])
        .map_err(|error| format!("开不了 worktree：{error}"))?;
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(Worktree { path, branch })
}

/// 原来目录里的 cwd 对应到 worktree 里（同一个子目录；没有就用 worktree 根）
pub fn map_cwd(cwd: &Path, workspace: &Path, worktree: &Path) -> PathBuf {
    let top = git(workspace, &["rev-parse", "--show-toplevel"]).map(PathBuf::from).unwrap_or_else(|_| workspace.to_path_buf());
    let top = std::fs::canonicalize(&top).unwrap_or(top);
    match cwd.strip_prefix(&top) {
        Ok(relative) if worktree.join(relative).is_dir() => worktree.join(relative),
        _ => worktree.to_path_buf(),
    }
}
