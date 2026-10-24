//! `--worktree`：开一个 git worktree 去那里干（同 Claude Code：要并行又互不干扰时主动选，不是有人在干活就自动开）：
//! `<仓库根>/.edith/worktrees/<编号>`，分支 `edith/<编号>`，从 HEAD 开（没提交的改动不带过去）。
//! 退出时没改东西（没有未提交的改动、分支上没有新提交）就自动删掉 worktree 和分支；改了的留着，由用户合并。
//! `.edith/worktrees/` 和 `.edith/*.local.json` 写进仓库的 info/exclude（项目配置 `.edith/*.json` 照常可以提交）。

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
    /// 从哪个提交开的（退出时看分支上有没有新提交）
    base: String,
    top: PathBuf,
}

/// 在 workspace 所在的仓库开一个 worktree；不是 git 仓库（或没有提交）返回原因
pub fn create(workspace: &Path) -> Result<Worktree, String> {
    let top = PathBuf::from(git(workspace, &["rev-parse", "--show-toplevel"]).map_err(|_| "不是 git 仓库".to_string())?);
    let base = git(&top, &["rev-parse", "--verify", "HEAD"]).map_err(|_| "仓库还没有提交，开不了 worktree".to_string())?;
    // worktree 和只给自己的配置不进 git
    let common = PathBuf::from(git(&top, &["rev-parse", "--git-common-dir"])?);
    let common = if common.is_absolute() { common } else { top.join(common) };
    let exclude = common.join("info").join("exclude");
    let mut current = std::fs::read_to_string(&exclude).unwrap_or_default();
    for rule in [".edith/worktrees/", ".edith/*.local.json"] {
        if !current.lines().any(|line| line.trim() == rule) {
            if !current.is_empty() && !current.ends_with('\n') {
                current.push('\n');
            }
            current.push_str(rule);
            current.push('\n');
        }
    }
    let _ = std::fs::create_dir_all(common.join("info"));
    let _ = std::fs::write(&exclude, current);
    let name = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
    let path = top.join(".edith").join("worktrees").join(&name);
    let branch = format!("edith/{name}");
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|error| error.to_string())?;
    git(&top, &["worktree", "add", "-q", "-b", &branch, &path.to_string_lossy(), "HEAD"])
        .map_err(|error| format!("开不了 worktree：{error}"))?;
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(Worktree { path, branch, base, top })
}

/// 退出时：没改东西就删掉 worktree 和分支；返回给用户看的一句话
pub fn finish(tree: &Worktree) -> String {
    let dirty = git(&tree.path, &["status", "--porcelain"]).map(|status| !status.is_empty()).unwrap_or(true);
    let ahead =
        git(&tree.top, &["rev-list", "--count", &format!("{}..{}", tree.base, tree.branch)]).map(|count| count != "0").unwrap_or(true);
    if !dirty && !ahead {
        let removed = git(&tree.top, &["worktree", "remove", "--force", &plain(&tree.path).to_string_lossy()]).is_ok()
            && git(&tree.top, &["branch", "-D", &tree.branch]).is_ok();
        if removed {
            return format!("没改东西：worktree 和分支 {} 清掉了", tree.branch);
        }
    }
    let hint = if dirty { "（有没提交的改动，先去那里提交）" } else { "" };
    format!("改动在 worktree {}、分支 {}{hint}：合并用 git merge {}", tree.path.display(), tree.branch, tree.branch)
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
