//! 数据和配置（同 Claude Code 的做法）：
//! - 全局 `~/.edith`（`EDITH_HOME` 换地方）：模型、提示词、全局配置、在跑的大脑登记（instances/）、下载的工具（tools/）；
//! - 每个工作目录的**数据**也在全局：`~/.edith/projects/<工作目录的路径，不是字母数字的换成 ->-<路径哈希前 8 位>/`（会话 sessions/、日志 logs/），
//!   项目目录里不放会话——同一个目录开几个大脑各写各的会话文件，worktree 删了对话也不丢；
//! - 项目目录 `<工作目录>/.edith` 只放**配置**：`*.json` 提交进仓库大家共用，`*.local.json` 只给自己（不提交）。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value, json};

/// 用户的家目录（Unix 的 HOME、Windows 的 USERPROFILE）
pub fn user_home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// 全局数据目录
pub fn dir() -> PathBuf {
    match std::env::var_os("EDITH_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => user_home().join(".edith"),
    }
}

static WORKSPACE: OnceLock<PathBuf> = OnceLock::new();

/// 定下这次的工作目录（启动时一次）
pub fn set_workspace(dir: PathBuf) {
    let _ = WORKSPACE.set(dir);
}

/// 工作目录：这个大脑管的那个目录（没定过是当前目录）
pub fn workspace() -> PathBuf {
    WORKSPACE.get().cloned().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// 项目这一层的配置：`<工作目录>/.edith`
pub fn workspace_data() -> PathBuf {
    workspace().join(".edith")
}

/// 这个工作目录的数据（会话、日志）：`~/.edith/projects/<可读的名字>-<哈希>`。
/// 可读的名字：路径里不是字母数字的换成 -（中文等字母照留），太长的留后面 60 个字；
/// 哈希：完整路径的 SHA-256 前 8 位——只换字符会撞（`/x/项目 一` 和 `/x/项目-一`、`a/b-c` 和 `a/b/c`）
pub fn project_dir() -> PathBuf {
    dir().join("projects").join(project_key(&workspace()))
}

pub fn project_key(workspace: &Path) -> String {
    use sha2::Digest;
    let path = workspace.to_string_lossy().into_owned();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path).to_string();
    let readable: Vec<char> = path.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let readable: String = readable[readable.len().saturating_sub(60)..].iter().collect();
    let digest = sha2::Sha256::digest(path.as_bytes());
    let hash: String = digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{hash}", readable.trim_matches('-'))
}

/// 从 start 往上找工作目录：先碰到有 .edith/ 的目录就是它（全局数据目录那个不算）；碰到 git 仓库的根（有 .git，
/// git worktree 的根是个 .git 文件）就停在那里——每个 worktree 自成一个工作目录，不会找到外面主仓库的 .edith；
/// 都没有用 start 本身
pub fn find_workspace(start: &Path) -> PathBuf {
    let global = std::fs::canonicalize(dir()).unwrap_or_else(|_| dir());
    let mut current = Some(start);
    while let Some(candidate) = current {
        let data = candidate.join(".edith");
        if data.is_dir() && std::fs::canonicalize(&data).unwrap_or(data) != global {
            return candidate.to_path_buf();
        }
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
        current = candidate.parent();
    }
    start.to_path_buf()
}

/// 一个工作目录的键（登记、套接字的文件名用）：完整路径的 SHA-256 前 16 位
pub fn workspace_key(workspace: &Path) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(workspace.to_string_lossy().as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// 先写临时文件再换名（写到一半退出不留半份）
pub fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("建不了 {}：{error}", parent.display()))?;
    }
    friday_core::writer::write_now(path, text.as_bytes()).map_err(|error| format!("写不了 {}：{error}", path.display()))
}

/// 大脑用的模型（OpenAI 兼容接口）
#[derive(Debug, Clone)]
pub struct Model {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// none / toggle / effort / thinkingType（== Friday 的 ThinkingSupport；不写按地址推断）
    pub thinking: String,
    pub enable_thinking: bool,
    pub temperature: f64,
    pub supports_vision: bool,
    /// 额外请求参数（JSON 对象的文本，盖掉同名的）
    pub extra_body: String,
}

impl Model {
    /// config.json 的值，环境变量 EDITH_BASE_URL / EDITH_API_KEY / EDITH_MODEL 盖掉同名的
    pub fn load() -> Result<Model, String> {
        let path = dir().join("config.json");
        let config = read_json(&path).unwrap_or_else(|| json!({}));
        let text = |key: &str, variable: &str| {
            std::env::var(variable)
                .ok()
                .filter(|value| !value.is_empty())
                .or_else(|| config.get(key).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default()
        };
        let base_url = text("baseURL", "EDITH_BASE_URL").trim_end_matches('/').to_string();
        let model = text("model", "EDITH_MODEL");
        if base_url.is_empty() || model.is_empty() {
            return Err(format!(
                "还不知道用哪个模型：在 {} 里写 {{\"baseURL\": \"https://…/v1\", \"apiKey\": \"…\", \"model\": \"…\"}}，\
                 或设环境变量 EDITH_BASE_URL、EDITH_API_KEY、EDITH_MODEL",
                path.display()
            ));
        }
        let thinking = config
            .get("thinking")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| friday_core::engines::inferred_thinking(&base_url, false).to_string());
        Ok(Model {
            api_key: text("apiKey", "EDITH_API_KEY"),
            thinking,
            enable_thinking: config.get("enableThinking").and_then(Value::as_bool).unwrap_or(false),
            temperature: config.get("temperature").and_then(Value::as_f64).unwrap_or(0.7),
            supports_vision: config.get("supportsVision").and_then(Value::as_bool).unwrap_or(false),
            extra_body: config
                .get("extraBody")
                .map(|extra| if extra.is_string() { extra.as_str().unwrap().to_string() } else { extra.to_string() })
                .unwrap_or_default(),
            base_url,
            model,
        })
    }

    /// 记「不接受的参数」、上下文上限用的键
    pub fn engine_id(&self) -> String {
        format!("edith|{}|{}", self.base_url, self.model)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn project_keys_do_not_collide() {
        let a = super::project_key(Path::new("/Users/x/项目一"));
        let b = super::project_key(Path::new("/Users/x/项目二"));
        assert!(a.starts_with("Users-x-项目一-") && a != b, "{a} {b}");
        assert_ne!(super::project_key(Path::new("/a/b-c")), super::project_key(Path::new("/a/b/c")));
        assert_eq!(super::project_key(Path::new("/a/b")), super::project_key(Path::new("/a/b")));
        let long = super::project_key(Path::new(&format!("/{}", "长".repeat(200))));
        assert!(long.chars().count() <= 60 + 9, "{long}");
    }
}
